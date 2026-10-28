//! Bounded, incremental reading of JSONL records without interpreting provider semantics.

use std::fmt;
use std::io::{self, BufRead};

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

/// Reads from a caller-supplied buffered source; this type does not open or write files.
pub struct CodexReader<R> {
    source: R,
    max_bytes: usize,
    buffer: Vec<u8>,
    offset: u64,
    line_offset: u64,
    too_large: bool,
    incomplete_reported: bool,
}

impl<R: BufRead> CodexReader<R> {
    pub fn new(source: R, max_bytes: usize) -> Self {
        Self {
            source,
            max_bytes,
            buffer: Vec::new(),
            offset: 0,
            line_offset: 0,
            too_large: false,
            incomplete_reported: false,
        }
    }

    /// Returns one outcome. A partial line is retained and retried when more bytes arrive.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        loop {
            let available = self.source.fill_buf()?;
            if available.is_empty() {
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
                if data_len > self.max_bytes.saturating_sub(self.buffer.len()) {
                    self.buffer.clear();
                    self.too_large = true;
                } else {
                    self.buffer.extend_from_slice(&available[..data_len]);
                }
            }
            self.source.consume(consumed);
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
                    match serde_json::from_slice(&self.buffer) {
                        Ok(value) => Record::Json { offset, value },
                        Err(_) => Record::Invalid { offset },
                    }
                };
                self.buffer.clear();
                self.too_large = false;
                return Ok(Some(result));
            }
        }
    }
}
