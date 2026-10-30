use serde_json::{Value, json};

pub fn message(kind: &str, uuid: &str, content: Value) -> Value {
    json!({"type":kind,"uuid":uuid,"sessionId":"session", "cwd":"/projects/demo", "version":"2.1", "timestamp":"2026-10-30T10:00:00Z", "message":{"id":"shared", "role":kind, "model":"claude-test", "content":content}})
}
