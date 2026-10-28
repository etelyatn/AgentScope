use agentscope::codex_reader::{CodexReader, Record};
use std::io::Cursor;

#[test]
fn reads_known_and_unknown_records_in_order_with_offsets() {
    let input = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic\"}}\n{\"type\":\"future_record\"}\n";
    let mut reader = CodexReader::new(Cursor::new(input), 1024);
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
    let mut reader = CodexReader::new(Cursor::new(b"not json\n{\"type\":\"event_msg\"}\n"), 1024);
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
    let mut reader = CodexReader::new(Cursor::new(b"{\"type\":\"partial\"}"), 1024);
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(reader.next_record().unwrap().is_none());
}

#[test]
fn oversized_records_are_skipped_without_losing_the_next_line() {
    let mut reader = CodexReader::new(Cursor::new(b"123456789\n{}\n"), 4);
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
    let mut reader = CodexReader::new(Cursor::new(b"secret\xff\n{}\n"), 1024);
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
    use std::fs::{self, OpenOptions};
    use std::io::{BufReader, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agentscope-reader-{}-{unique}.jsonl",
        std::process::id()
    ));
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    writer.write_all(b"{\"type\":\"event").unwrap();
    let file = OpenOptions::new().read(true).open(&path).unwrap();
    let mut reader = CodexReader::new(BufReader::new(file), 1024);
    assert!(matches!(
        reader.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(reader.next_record().unwrap().is_none());
    writer.write_all(b"_msg\"}\n").unwrap();
    assert!(
        matches!(reader.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["type"] == "event_msg")
    );
    drop(reader);
    drop(writer);
    fs::remove_file(path).unwrap();
}

#[test]
fn debug_output_does_not_include_valid_record_content() {
    let mut reader = CodexReader::new(
        Cursor::new(b"{\"secret\":\"synthetic-private-value\"}\n"),
        1024,
    );
    let record = reader.next_record().unwrap().unwrap();
    assert!(matches!(record, Record::Json { .. }));
    assert!(!format!("{record:?}").contains("synthetic-private-value"));
}
