//! A three-provider synthetic "home directory" with credential decoys, used by the
//! read-only, privacy, and acceptance suites. Everything here is synthetic.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::codex as cx;
use super::opencode as oc;
use super::{dated, write_lines};

/// Planted in every credential decoy. It must never reach any output.
pub const SENTINEL: &str = "SENTINEL-CRED-7f3a91";
/// Planted in transcript text in credential-looking shapes. It must be masked.
pub const INLINE_SECRET: &str = "SENTINEL-INLINE-c0ffee42";

pub const CODEX_ID: &str = "codex-ac-1";
pub const CLAUDE_ID: &str = "claude-ac-1";
pub const OPENCODE_ID: &str = "ses_ac1";

pub struct World {
    pub dir: tempfile::TempDir,
    pub codex_root: PathBuf,
    pub claude_root: PathBuf,
    pub db: PathBuf,
    /// Held open like a live OpenCode writer (autocheckpoint off), so rows stay in the WAL.
    pub writer: Connection,
}

pub fn claude_line(
    kind: &str,
    uuid: &str,
    ts: &str,
    content: Value,
    usage: Option<Value>,
) -> String {
    let mut v = json!({"type": kind, "uuid": uuid, "sessionId": CLAUDE_ID, "cwd": "/work/claude",
        "version": "2.1.311", "timestamp": ts,
        "message": {"id": uuid, "role": kind, "model": "claude-test", "content": content}});
    if let Some(u) = usage {
        v["message"]["usage"] = u;
    }
    v.to_string()
}

impl World {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let codex_root = dir.path().join("codex").join("sessions");
        let claude_root = dir.path().join("claude").join("projects");
        let db = dir.path().join("opencode").join("opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();

        // Codex: prompt, assistant (with an inline secret and a control sequence),
        // a function call/result pair, usage, completion.
        write_lines(
            &dated(&codex_root, "rollout-ac.jsonl"),
            &[
                cx::session_meta("2026-10-29T23:50:00Z", CODEX_ID, "/work/codex"),
                cx::turn_context("2026-10-29T23:50:01Z", "gpt-test", "/work/codex"),
                cx::user("2026-10-29T23:50:02Z", "Codex prompt: add inventory checks"),
                cx::assistant(
                    "2026-10-29T23:50:03Z",
                    &format!("done \u{1b}]0;pwned\u{7} api_key={INLINE_SECRET}"),
                ),
                cx::item_completed(
                    "2026-10-29T23:50:04Z",
                    cx::command_item("c1", "ls", "inventory.rs\nmain.rs", 0),
                ),
                cx::usage_record("2026-10-29T23:50:06Z", cx::usage(100, 40, 20, 5, 120)),
                cx::task_complete("2026-10-29T23:50:07Z"),
            ],
        );
        // Credential decoys: never read, never discovered.
        fs::write(
            codex_root.join("auth.json"),
            format!("{{\"token\":\"{SENTINEL}\"}}"),
        )
        .unwrap();
        fs::write(
            dated(&codex_root, "auth.json"),
            format!("{{\"token\":\"{SENTINEL}\"}}"),
        )
        .unwrap();

        // Claude: prompt, assistant tool_use with usage, tool_result, final answer.
        write_lines(
            &claude_root.join("proj").join("claude-ac-1.jsonl"),
            &[
                claude_line(
                    "user",
                    "u1",
                    "2026-10-29T23:51:00Z",
                    json!("Claude prompt: refactor"),
                    None,
                ),
                claude_line(
                    "assistant",
                    "a1",
                    "2026-10-29T23:51:01Z",
                    json!([{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]),
                    Some(
                        json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":2}),
                    ),
                ),
                claude_line(
                    "user",
                    "u2",
                    "2026-10-29T23:51:02Z",
                    json!([{"type":"tool_result","tool_use_id":"t1","content":"file body"}]),
                    None,
                ),
                claude_line(
                    "assistant",
                    "a2",
                    "2026-10-29T23:51:03Z",
                    json!("all done"),
                    None,
                ),
            ],
        );
        fs::write(
            claude_root.join(".credentials.json"),
            format!("{{\"k\":\"{SENTINEL}\"}}"),
        )
        .unwrap();
        fs::write(
            claude_root.join("proj").join(".credentials.json"),
            format!("{{\"k\":\"{SENTINEL}\"}}"),
        )
        .unwrap();

        // OpenCode: v2 database with credential tables seeded by the fixture schema.
        let writer = oc::create_v2(&db);
        let mut s = oc::Session::new(OPENCODE_ID, NOW_MS - 600_000, NOW_MS - 60_000);
        s.title = Some("OpenCode session");
        oc::insert_session(&writer, &s);
        writer
            .execute(
                "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
                 VALUES ('m1', ?1, 'user', 0, ?2, ?2, ?3)",
                rusqlite::params![
                    OPENCODE_ID,
                    NOW_MS - 590_000,
                    json!({"text": "OpenCode prompt"}).to_string()
                ],
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO credential VALUES ('k2', ?1)",
                rusqlite::params![SENTINEL],
            )
            .unwrap();

        World {
            dir,
            codex_root,
            claude_root,
            db,
            writer,
        }
    }

    pub fn config(&self) -> Config {
        let cli = CliOverrides {
            codex_sessions: Some(self.codex_root.clone()),
            claude_projects: Some(self.claude_root.clone()),
            opencode_db: Some(self.db.clone()),
            no_processes: true,
            ..CliOverrides::default()
        };
        Config::load_with_env(&cli, &Env::new()).expect("valid world config")
    }

    pub fn readers(&self) -> Vec<Box<dyn ProviderReader>> {
        let config = self.config();
        vec![
            Box::new(CodexScanner::new(&config).expect("codex")),
            Box::new(ClaudeScanner::new(&config).expect("claude")),
            Box::new(OpenCodeScanner::new(&config).expect("opencode")),
        ]
    }

    pub fn codex_file(&self) -> PathBuf {
        dated(&self.codex_root, "rollout-ac.jsonl")
    }

    pub fn claude_file(&self) -> PathBuf {
        self.claude_root.join("proj").join("claude-ac-1.jsonl")
    }
}

pub const NOW_MS: i64 = super::NOW;

/// Content snapshot of every regular file under `root` (path relative to `root`).
/// SQLite `-shm`/`-wal` sidecars are reported separately by [`Snapshot::sidecars`].
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    pub sidecars: BTreeMap<PathBuf, Vec<u8>>,
}

fn is_sidecar(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with("-shm") || name.ends_with("-wal") || name.ends_with("-journal")
}

pub fn snapshot(root: &Path) -> Snapshot {
    fn walk(dir: &Path, root: &Path, out: &mut Snapshot) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let bytes = fs::read(&path).unwrap_or_default();
                if is_sidecar(&path) {
                    out.sidecars.insert(rel, bytes);
                } else {
                    out.files.insert(rel, bytes);
                }
            }
        }
    }
    let mut out = Snapshot {
        files: BTreeMap::new(),
        sidecars: BTreeMap::new(),
    };
    walk(root, root, &mut out);
    out
}

/// Names (relative) of every directory entry, including directories, for new-file detection.
pub fn all_paths(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

use agentscope::domain::{
    PageAnchor, PageDirection, PageRequest, RefreshBudget, SessionSummary, TranscriptPage, Warning,
};

pub const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};
pub const PAGE: PageRequest = PageRequest {
    max_events: 1000,
    max_bytes: 10_000_000,
};

/// Everything a provider surfaced for one pass.
#[derive(Default)]
pub struct Surface {
    pub sessions: Vec<SessionSummary>,
    pub warnings: Vec<Warning>,
    pub pages: Vec<TranscriptPage>,
}

/// Refresh every reader to completion once.
pub fn refresh_all(readers: &mut [Box<dyn ProviderReader>], out: &mut Surface) {
    for r in readers.iter_mut() {
        loop {
            let delta = r.refresh(super::NOW, BIG).expect("refresh");
            out.sessions.extend(delta.sessions);
            out.warnings.extend(delta.warnings);
            if delta.scan.done {
                break;
            }
        }
    }
}

/// Refresh, then open every session: all pages both directions, every event's source
/// record, every diagnostic's source record, and new-event counting.
pub fn exercise_everything(readers: &mut [Box<dyn ProviderReader>]) -> Surface {
    let mut out = Surface::default();
    refresh_all(readers, &mut out);
    let sessions = out.sessions.clone();
    for s in &sessions {
        let r = readers
            .iter_mut()
            .find(|r| r.provider() == s.key.provider)
            .expect("reader");
        for direction in [PageDirection::Forward, PageDirection::Backward] {
            let page = r
                .transcript_page(&s.key, None, direction, PAGE)
                .expect("page");
            for e in &page.events {
                let _ = r.source_record(&e.provenance.locator);
            }
            for d in &page.diagnostics {
                let _ = r.source_record(&d.locator);
            }
            if let Some(first) = page.events.first() {
                let anchor = PageAnchor {
                    timestamp: first.timestamp,
                    source_order: first.source_order,
                    stable_id: first.id.clone(),
                };
                let _ = r.count_events_after(&s.key, &anchor);
            }
            out.pages.push(page);
        }
    }
    out
}
