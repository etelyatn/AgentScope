//! Bounded, incremental reading of JSONL files without interpreting provider semantics.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::SystemTime;

/// Streaming FNV-1a 64: split-independent, so hashing in different chunk sizes
/// agrees. Integrity check for "is this still the file I read", not cryptographic.
#[derive(Clone, Copy)]
pub struct ContentHash(u64);

impl ContentHash {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for ContentHash {
    fn default() -> Self {
        Self::new()
    }
}

/// A complete JSON value or a content-free indication of an unusable line.
pub enum Record {
    Json {
        offset: u64,
        value: serde_json::Value,
    },
    Invalid {
        offset: u64,
    },
    TooLarge {
        offset: u64,
    },
    Incomplete {
        offset: u64,
    },
}

// Avoid leaking transcript content through routine debug output.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json { offset, .. } => f
                .debug_struct("Json")
                .field("offset", offset)
                .finish_non_exhaustive(),
            Self::Invalid { offset } => f.debug_struct("Invalid").field("offset", offset).finish(),
            Self::TooLarge { offset } => {
                f.debug_struct("TooLarge").field("offset", offset).finish()
            }
            Self::Incomplete { offset } => f
                .debug_struct("Incomplete")
                .field("offset", offset)
                .finish(),
        }
    }
}

/// Strongest portable file identity: Unix `(dev, ino)`; Windows `creation_time`.
/// Windows length and last-write time are tracked separately in [`Stamp`].
#[cfg(unix)]
type FileKey = (u64, u64);
#[cfg(windows)]
type FileKey = u64;
#[cfg(not(any(unix, windows)))]
type FileKey = ();

#[cfg(unix)]
fn file_key(meta: &fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(windows)]
fn file_key(meta: &fs::Metadata) -> FileKey {
    use std::os::windows::fs::MetadataExt;
    meta.creation_time()
}

#[cfg(not(any(unix, windows)))]
fn file_key(_meta: &fs::Metadata) -> FileKey {}

/// Identity plus the length and mtime observed at the last check.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    key: FileKey,
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            key: file_key(meta),
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// Incremental line reader over a growing file. A trailing partial line is held and
/// retried once more bytes arrive. Rotation (identity change, shrink, or an mtime change
/// without growth) resets the cursor to the start of the file and is counted.
///
/// Known false negative: replacing a file in place with same-length content and the same
/// mtime (and, on Windows, the same creation time) is indistinguishable from no change.
pub struct JsonlCursor {
    path: PathBuf,
    reader: BufReader<File>,
    max_bytes: usize,
    buffer: Vec<u8>,
    /// Bytes consumed from the file, including the pending partial line.
    offset: u64,
    line_offset: u64,
    too_large: bool,
    incomplete_reported: bool,
    at_eof: bool,
    stamp: Stamp,
    rotations: u64,
    hash: ContentHash,
}

fn classify(line: &[u8], offset: u64, max_bytes: usize) -> Record {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > max_bytes {
        return Record::TooLarge { offset };
    }
    match serde_json::from_slice(line) {
        Ok(value) => Record::Json { offset, value },
        Err(_) => Record::Invalid { offset },
    }
}

impl JsonlCursor {
    pub fn open(path: PathBuf, max_record_bytes: usize) -> io::Result<Self> {
        let file = File::open(&path)?;
        let stamp = Stamp::of(&file.metadata()?);
        Ok(Self {
            path,
            reader: BufReader::new(file),
            max_bytes: max_record_bytes,
            buffer: Vec::new(),
            offset: 0,
            line_offset: 0,
            too_large: false,
            incomplete_reported: false,
            at_eof: false,
            stamp,
            rotations: 0,
            hash: ContentHash::new(),
        })
    }

    /// Bytes consumed from the file so far, including a held partial line.
    pub fn position(&self) -> u64 {
        self.offset
    }

    /// Hash of every byte consumed so far (including a held partial line), reset on
    /// rotation. Right after a record is returned it covers exactly `[0, position)`.
    pub fn content_hash(&self) -> u64 {
        self.hash.finish()
    }

    /// Number of times the source was detected as rotated or truncated and re-read.
    pub fn rotation_count(&self) -> u64 {
        self.rotations
    }

    /// Whether the current path still names the file held by this cursor.
    pub(crate) fn same_file(&self) -> io::Result<bool> {
        let held = same_file::Handle::from_file(self.reader.get_ref().try_clone()?)?;
        let current = same_file::Handle::from_path(&self.path)?;
        Ok(held == current)
    }

    fn check_rotation(&mut self) -> io::Result<()> {
        // Sample the path first, then the held handle: a writer appending between the two
        // samples can only make the handle look larger, never the path.
        let current = Stamp::of(&fs::metadata(&self.path)?);
        let held = Stamp::of(&self.reader.get_ref().metadata()?);
        let rotated = path_replaced(current, held, self.offset)
            || (current.len == self.stamp.len && current.modified != self.stamp.modified);
        if rotated {
            // Open first; commit stamp and handle together so a failed reopen is retried.
            let file = File::open(&self.path)?;
            let stamp = Stamp::of(&file.metadata()?);
            self.stamp = stamp;
            self.reader = BufReader::new(file);
            self.buffer.clear();
            self.offset = 0;
            self.line_offset = 0;
            self.too_large = false;
            self.incomplete_reported = false;
            self.rotations += 1;
            self.hash = ContentHash::new();
        } else {
            self.stamp = current;
        }
        Ok(())
    }

    /// Returns one outcome. A partial line is retained and retried when more bytes arrive.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        if self.at_eof {
            self.check_rotation()?;
            self.at_eof = false;
        }
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                self.at_eof = true;
                if (self.too_large || !self.buffer.is_empty()) && !self.incomplete_reported {
                    self.incomplete_reported = true;
                    return Ok(Some(Record::Incomplete {
                        offset: self.line_offset,
                    }));
                }
                return Ok(None);
            }
            let newline = available.iter().position(|&byte| byte == b'\n');
            let data_len = newline.unwrap_or(available.len());
            let consumed = data_len + usize::from(newline.is_some());
            if !self.too_large {
                // One byte of slack so a CRLF terminator is not counted against the record.
                if data_len
                    > self
                        .max_bytes
                        .saturating_add(1)
                        .saturating_sub(self.buffer.len())
                {
                    self.buffer.clear();
                    self.too_large = true;
                } else {
                    self.buffer.extend_from_slice(&available[..data_len]);
                }
            }
            self.hash.update(&available[..consumed]);
            self.reader.consume(consumed);
            self.offset = self
                .offset
                .checked_add(consumed as u64)
                .ok_or_else(|| io::Error::other("source offset overflow"))?;
            self.incomplete_reported = false;
            if newline.is_some() {
                let offset = self.line_offset;
                self.line_offset = self.offset;
                let result = if self.too_large {
                    Record::TooLarge { offset }
                } else {
                    classify(&self.buffer, offset, self.max_bytes)
                };
                self.buffer.clear();
                self.too_large = false;
                return Ok(Some(result));
            }
        }
    }

    /// Reads the line starting at `offset` (without its terminator), truncated to
    /// `max_bytes`. Returns `None` at or past end of file. Does not move the cursor.
    pub fn read_at(&mut self, offset: u64, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        let mut file = BufReader::new(File::open(&self.path)?);
        if offset >= file.get_ref().metadata()?.len() {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut line = Vec::new();
        (&mut file)
            .take(max_bytes as u64)
            .read_until(b'\n', &mut line)?;
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Some(line))
    }

    /// Returns up to `max_records` of the newest complete lines, oldest first, examining at
    /// most about `max_bytes` from the end of the file. A partial trailing line and a
    /// partial leading line are excluded. Does not move the cursor.
    pub fn tail_records(
        &mut self,
        max_records: usize,
        max_bytes: usize,
    ) -> io::Result<Vec<Record>> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(max_bytes as u64);
        let read_from = start.saturating_sub(1);
        file.seek(SeekFrom::Start(read_from))?;
        let mut data = Vec::new();
        file.take(len - read_from).read_to_end(&mut data)?;

        Ok(tail_from_window(
            data,
            read_from,
            start > 0,
            max_records,
            self.max_bytes,
        ))
    }
}

/// True if the path (sampled before `held`) no longer refers to the held file. Growth is
/// not replacement; this also catches Windows "tunneled" replacements that keep the
/// creation time but are larger than the file we hold.
fn path_replaced(path: Stamp, held: Stamp, consumed: u64) -> bool {
    path.key != held.key
        || path.len < consumed
        || path.len > held.len
        || (path.len == held.len && path.modified != held.modified)
}

/// Parses complete lines from a window read at `read_from`. When `has_lead`, `data[0]` is
/// the byte before the window proper. A short or empty buffer (the file was truncated
/// while being read) yields no records rather than panicking.
fn tail_from_window(
    mut data: Vec<u8>,
    read_from: u64,
    has_lead: bool,
    max_records: usize,
    max_record_bytes: usize,
) -> Vec<Record> {
    let mut base = read_from;
    if has_lead {
        match data.first() {
            None => return Vec::new(),
            Some(b'\n') => {
                data.remove(0);
                base += 1;
            }
            Some(_) => {
                let Some(pos) = data.iter().position(|&b| b == b'\n') else {
                    return Vec::new();
                };
                data.drain(..=pos);
                base += pos as u64 + 1;
            }
        }
    }
    let Some(end) = data.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let mut records = Vec::new();
    let mut line_start = 0usize;
    for (i, _) in data[..=end]
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
    {
        records.push(classify(
            &data[line_start..i],
            base + line_start as u64,
            max_record_bytes,
        ));
        line_start = i + 1;
    }
    let skip = records.len().saturating_sub(max_records);
    records.drain(..skip);
    records
}

#[cfg(test)]
mod tests {
    use super::{FileKey, Stamp, path_replaced, tail_from_window};
    use std::time::{Duration, SystemTime};

    #[test]
    fn append_between_samples_is_not_a_replacement() {
        let key: FileKey = Default::default();
        let t = SystemTime::UNIX_EPOCH;
        let path = Stamp {
            key,
            len: 10,
            modified: Some(t),
        };
        let held = Stamp {
            key,
            len: 15,
            modified: Some(t + Duration::from_secs(1)),
        };
        assert!(!path_replaced(path, held, 10));
        // A larger path than the held file is a replacement.
        assert!(path_replaced(held, path, 10));
        // Shrink below the consumed offset is a replacement.
        assert!(path_replaced(path, held, 11));
    }

    #[test]
    fn truncated_tail_window_returns_nothing_instead_of_panicking() {
        assert!(tail_from_window(Vec::new(), 99, true, 5, 1024).is_empty());
        assert!(tail_from_window(b"x".to_vec(), 99, true, 5, 1024).is_empty());
    }
}
