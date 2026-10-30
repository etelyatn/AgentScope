//! Codex rollout JSONL fixture builder (synthetic records only).
#![allow(dead_code)]

use serde_json::{Value, json};

pub fn record(ts: &str, kind: &str, payload: Value) -> String {
    json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
}

pub fn session_meta(ts: &str, id: &str, cwd: &str) -> String {
    record(
        ts,
        "session_meta",
        json!({"id": id, "cwd": cwd, "originator": "codex_cli_rs", "cli_version": "0.159.2", "source": "cli"}),
    )
}

pub fn session_meta_with(ts: &str, extra: Value) -> String {
    record(ts, "session_meta", extra)
}

pub fn turn_context(ts: &str, model: &str, cwd: &str) -> String {
    record(ts, "turn_context", json!({"model": model, "cwd": cwd}))
}

fn message(ts: &str, role: &str, kind: &str, text: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]}),
    )
}

pub fn user(ts: &str, text: &str) -> String {
    message(ts, "user", "input_text", text)
}

pub fn developer(ts: &str, text: &str) -> String {
    message(ts, "developer", "input_text", text)
}

pub fn assistant(ts: &str, text: &str) -> String {
    message(ts, "assistant", "output_text", text)
}

pub fn task_started(ts: &str, window: Option<u64>) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "task_started", "model_context_window": window}),
    )
}

pub fn task_complete(ts: &str) -> String {
    record(ts, "event_msg", json!({"type": "task_complete"}))
}

pub fn usage(input: u64, cached: u64, output: u64, reasoning: u64, total: u64) -> Value {
    json!({
        "input_tokens": input,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": 0,
        "output_tokens": output,
        "reasoning_output_tokens": reasoning,
        "total_tokens": total,
    })
}

pub fn usage_record(ts: &str, thread: Value) -> String {
    record(
        ts,
        "token_usage_record",
        json!({"thread_token_usage": thread, "turn_token_usage": thread, "usage": thread}),
    )
}

pub fn token_count(ts: &str, total: Value, window: Option<u64>) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "token_count", "info": {
            "total_token_usage": total,
            "last_token_usage": total,
            "model_context_window": window,
        }}),
    )
}
