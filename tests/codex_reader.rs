use agentscope::ingest::jsonl::{JsonlCursor, Record};

fn reader(input: &[u8], max: usize) -> (tempfile::TempDir, JsonlCursor) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, input).unwrap();
    let cursor = JsonlCursor::open(path, max).unwrap();
    (dir, cursor)
}

#[test]
fn reads_known_and_unknown_records_in_order_with_offsets() {
    let input = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic\"}}\n{\"type\":\"future_record\"}\n";
    let (_dir, mut reader) = reader(input, 1024);
    let first = reader.next_record().unwrap().unwrap();
    assert!(
        matches!(first, Record::Json { offset: 0, ref value } if value["payload"]["id"] == "synthetic")
    );
    let second = reader.next_record().unwrap().unwrap();
    assert!(
        matches!(second, Record::Json { offset: 53, ref value } if value["type"] == "future_record")
    );
    assert!(reader.next_record().unwrap().is_none());
}

#[test]
fn malformed_records_do_not_hide_later_records() {
    let (_dir, mut reader) = reader(b"not json\n{\"type\":\"event_msg\"}\n", 1024);
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Invalid { offset: 0 })
    ));
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Json { offset: 9, .. })
    ));
}

#[test]
fn incomplete_trailing_line_is_reported_without_parsing_it() {
    let (_dir, mut reader) = reader(b"{\"type\":\"partial\"}", 1024);
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(reader.next_record().unwrap().is_none());
}

#[test]
fn oversized_records_are_skipped_without_losing_the_next_line() {
    let (_dir, mut reader) = reader(b"123456789\n{}\n", 4);
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::TooLarge { offset: 0 })
    ));
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Json { offset: 10, .. })
    ));
}

#[test]
fn invalid_utf8_is_rejected_without_disclosing_input() {
    let (_dir, mut reader) = reader(b"secret\xff\n{}\n", 1024);
    let outcome = reader.next_record().unwrap().unwrap();
    assert!(matches!(outcome, Record::Invalid { offset: 0 }));
    assert!(!format!("{outcome:?}").contains("secret"));
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Json { offset: 8, .. })
    ));
}

#[test]
fn appended_bytes_complete_a_previously_partial_record() {
    use std::fs::OpenOptions;
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    writer.write_all(b"{\"type\":\"event").unwrap();
    let mut reader = JsonlCursor::open(path, 1024).unwrap();
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(reader.next_record().unwrap().is_none());
    writer.write_all(b"_msg\"}\n").unwrap();
    assert!(
        matches!(reader.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["type"] == "event_msg")
    );
}

#[test]
fn debug_output_does_not_include_valid_record_content() {
    let (_dir, mut reader) = reader(b"{\"secret\":\"synthetic-private-value\"}\n", 1024);
    let record = reader.next_record().unwrap().unwrap();
    assert!(matches!(record, Record::Json { .. }));
    assert!(!format!("{record:?}").contains("synthetic-private-value"));
}
