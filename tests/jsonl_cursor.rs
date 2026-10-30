use agentscope::ingest::jsonl::{JsonlCursor, Record};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    (dir, path)
}

fn append(path: &PathBuf, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn holds_partial_line_until_newline_arrives() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(cursor.next_record().unwrap().is_none());
    append(&path, b"\n{\"b\":2}\n");
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["a"] == 1)
    );
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 8, ref value }) if value["b"] == 2)
    );
    assert!(cursor.next_record().unwrap().is_none());
}

#[test]
fn reports_invalid_oversized_and_shrink_without_stopping() {
    let (_dir, path) = temp();
    append(&path, b"nope\n123456789\n{\"ok\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 8).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Invalid { offset: 0 })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::TooLarge { offset: 5 })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 15, .. })
    ));
    assert!(cursor.next_record().unwrap().is_none());
    assert_eq!(cursor.rotation_count(), 0);

    fs::write(&path, b"{\"n\":1}\n").unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["n"] == 1)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

// Windows "file tunneling" can carry the old creation time onto a same-named replacement,
// so identity-only replacement is a Unix-only guarantee; Windows relies on shrink/mtime.
#[cfg(unix)]
#[test]
fn replacement_with_new_identity_resets_cursor() {
    let (dir, path) = temp();
    append(&path, b"{\"old\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    let other = dir.path().join("new.jsonl");
    fs::write(&other, b"{\"old\":1}\n{\"new\":2}\n").unwrap();
    fs::rename(&other, &path).unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["old"] == 1)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

#[test]
fn same_length_same_mtime_replacement_is_a_documented_false_negative() {
    let (_dir, path) = temp();
    append(&path, b"{\"v\":1}\n");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    (&file).write_all(b"{\"v\":2}\n").unwrap();
    file.set_modified(mtime).unwrap();
    drop(file);
    assert!(cursor.next_record().unwrap().is_none());
    assert_eq!(cursor.rotation_count(), 0);
}

#[test]
fn crlf_lines_are_parsed_and_offsets_count_every_byte() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\r\n{\"b\":2}\r\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 9, .. })
    ));
    assert_eq!(cursor.read_at(9, 1024).unwrap().unwrap(), b"{\"b\":2}");
}

#[test]
fn read_at_returns_the_record_bytes_for_source_view() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\n{\"b\":2}\nlast");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert_eq!(cursor.read_at(0, 1024).unwrap().unwrap(), b"{\"a\":1}");
    assert_eq!(cursor.read_at(8, 1024).unwrap().unwrap(), b"{\"b\":2}");
    assert_eq!(cursor.read_at(8, 3).unwrap().unwrap(), b"{\"b");
    assert_eq!(cursor.read_at(16, 1024).unwrap().unwrap(), b"last");
    assert!(cursor.read_at(20, 1024).unwrap().is_none());
    assert!(cursor.read_at(999, 1024).unwrap().is_none());
    // read_at must not disturb the sequential position.
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
}

#[test]
fn tail_records_returns_newest_complete_records_in_order() {
    let (_dir, path) = temp();
    append(&path, b"{\"n\":1}\n{\"n\":2}\nbad\n{\"n\":4}\n{\"n\":5");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    let tail = cursor.tail_records(3, 1024).unwrap();
    assert_eq!(tail.len(), 3);
    assert!(matches!(tail[0], Record::Json { offset: 8, ref value } if value["n"] == 2));
    assert!(matches!(tail[1], Record::Invalid { offset: 16 }));
    assert!(matches!(tail[2], Record::Json { offset: 20, ref value } if value["n"] == 4));
    // Byte budget cuts the oldest records and never yields a partial leading line.
    let small = cursor.tail_records(10, 14).unwrap();
    assert_eq!(small.len(), 1);
    assert!(matches!(small[0], Record::Json { offset: 20, .. }));
    // Tail does not move the sequential cursor.
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
}
