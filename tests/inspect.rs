use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn inspects_synthetic_messages_safely_and_reports_bad_records() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agentscope-inspect-{}-{unique}.jsonl",
        std::process::id()
    ));
    let data = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-id\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello\"}]}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi \\u001b[31m sk-test-secret password = synthetic-password --password synthetic-private-value --api-key synthetic-private-key --password = synthetic-spaced-secret Authorization: Bearer synthetic-bearer\"}]}}\n",
        "not json\n",
        "{\"type\":\"partial\"}"
    );
    fs::write(&path, data).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("inspect")
        .arg(&path)
        .output()
        .unwrap();
    let original = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success());
    assert_eq!(original, data.as_bytes());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("synthetic-id"));
    assert_eq!(stdout.matches("hello").count(), 1);
    assert!(stdout.contains("hi"));
    assert!(!stdout.contains('\u{1b}'));
    assert!(!stdout.contains("sk-test-secret"));
    assert!(!stdout.contains("synthetic-password"));
    assert!(!stdout.contains("synthetic-bearer"));
    assert!(!stdout.contains("synthetic-private-value"));
    assert!(!stdout.contains("synthetic-private-key"));
    assert!(!stdout.contains("synthetic-spaced-secret"));
    assert!(stderr.contains("Invalid"));
    assert!(stderr.contains("Incomplete"));
}

#[test]
fn missing_input_fails_without_displaying_the_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .args(["inspect", "missing-synthetic-private-file.jsonl"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("missing-synthetic-private-file")
    );
}
