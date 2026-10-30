use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentscope::domain::*;
use agentscope::processes::{
    ProcessError, ProcessInfo, ProcessSnapshot, associate, association_from_observation,
    candidates, identity_matches, observe_processes,
};

fn matches(program: &str, args: &[&str]) -> bool {
    let provider = match program {
        "codex" => Provider::Codex,
        "claude" => Provider::Claude,
        _ => Provider::OpenCode,
    };
    let mut argv = vec![program];
    argv.extend_from_slice(args);
    identity_matches(provider, &argv, "01a1") || identity_matches(provider, &argv, "ses_01a1")
}

fn session(provider: Provider, id: &str, cwd: Option<&str>) -> SessionSummary {
    SessionSummary {
        key: SessionKey {
            provider,
            provider_session_id: id.into(),
            source: SourceKey::File("a".into()),
        },
        title: "t".into(),
        title_derived: false,
        project: None,
        cwd: cwd.map(PathBuf::from),
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: None,
        last_event_at: None,
        last_persisted_at: None,
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 0,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::NotChecked,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn proc(pid: u32, exe: &str, cwd: Option<&str>, argv: Option<&[&str]>) -> ProcessInfo {
    ProcessInfo::new(
        pid,
        Some(1_000 + pid as i64),
        exe.into(),
        cwd.map(PathBuf::from),
        argv.map(|a| a.iter().map(|s| s.to_string()).collect()),
    )
}

fn snap(processes: Vec<ProcessInfo>) -> ProcessSnapshot {
    ProcessSnapshot {
        sampled_at: 5_000,
        processes,
    }
}

#[test]
fn matches_only_identity_bearing_arguments() {
    assert!(matches("codex", &["resume", "01a1"]));
    assert!(matches("codex", &["exec", "resume", "01a1"]));
    assert!(matches("claude", &["--resume", "01a1"]));
    assert!(matches("claude", &["-r", "01a1"]));
    assert!(matches("claude", &["--resume=01a1"]));
    assert!(matches("opencode", &["--session", "ses_01a1"]));
    assert!(matches("opencode", &["-s", "ses_01a1"]));
    assert!(matches("opencode", &["--session=ses_01a1"]));
    // Windows executable suffix and node wrapper.
    assert!(identity_matches(
        Provider::Codex,
        &["C:\\bin\\codex.exe", "resume", "01a1"],
        "01a1"
    ));
    assert!(identity_matches(
        Provider::OpenCode,
        &["node", "/x/bin/opencode", "-s", "ses_01a1"],
        "ses_01a1"
    ));
}

#[test]
fn session_id_inside_prompt_or_reference_is_not_a_match() {
    assert!(!matches("claude", &["-p", "summarize 01a1"]));
    assert!(!matches("claude", &["-p", "--resume 01a1"]));
    assert!(!matches("claude", &["--print", "-r=01a1"]));
    assert!(!matches("claude", &["--", "--resume", "01a1"]));
    assert!(!matches("claude", &["--resume", "01a1", "--fork-session"]));
    assert!(!matches("claude", &["--session-id", "01a1"]));
    assert!(!matches("codex", &["queue", "01a1"]));
    assert!(!matches("codex", &["archive", "01a1"]));
    assert!(!matches("codex", &["delete", "01a1"]));
    assert!(!matches("codex", &["fork", "01a1"]));
    assert!(!matches("codex", &["exec", "01a1"]));
    assert!(!matches("codex", &["exec", "resume 01a1"]));
    assert!(!matches("codex", &["exec", "summarize", "resume", "01a1"]));
    assert!(!matches("codex", &["resume", "x01a1"]));
    assert!(!matches("opencode", &["--continue", "01a1"]));
    assert!(!matches("opencode", &["run", "talk about ses_01a1"]));
    assert!(!matches("opencode", &["--session", "ses_01a1x"]));
    // Right arguments but a different program must not match.
    assert!(!matches("node", &["resume", "01a1"]));
    assert!(!identity_matches(
        Provider::Claude,
        &["codex", "resume", "01a1"],
        "01a1"
    ));
}

#[test]
fn candidates_never_become_matched_by_name_or_cwd() {
    let s = session(Provider::Codex, "01a1", Some("/work/proj"));
    let snapshot = snap(vec![
        proc(10, "/usr/bin/codex", Some("/work/proj"), Some(&["codex"])),
        proc(
            11,
            "codex",
            Some("/work/proj"),
            Some(&["codex", "resume", "other"]),
        ),
        proc(
            12,
            "claude",
            Some("/work/proj"),
            Some(&["claude", "--resume", "01a1"]),
        ),
        proc(13, "vim", Some("/work/proj"), Some(&["vim", "01a1"])),
    ]);
    assert_eq!(
        associate(&s, &snapshot),
        ProcessAssociation::Candidates { count: 2 }
    );
    assert_eq!(candidates(Provider::Codex, &snapshot).len(), 2);

    let none = snap(vec![proc(13, "vim", Some("/work/proj"), Some(&["vim"]))]);
    assert_eq!(associate(&s, &none), ProcessAssociation::Unmatched);
}

#[test]
fn match_picks_oldest_process_and_reports_start_time() {
    let s = session(Provider::Codex, "01a1", None);
    let snapshot = snap(vec![
        proc(30, "codex", None, Some(&["codex", "resume", "01a1"])),
        proc(
            20,
            "codex",
            None,
            Some(&["codex", "exec", "resume", "01a1"]),
        ),
    ]);
    assert_eq!(
        associate(&s, &snapshot),
        ProcessAssociation::Matched {
            pid: 20,
            started_at: Some(1_020)
        }
    );
}

#[test]
fn permission_failure_is_unavailable_not_not_found() {
    let s = session(Provider::Codex, "01a1", None);
    let failed: Result<ProcessSnapshot, ProcessError> = Err(ProcessError::Unavailable);
    assert_eq!(
        association_from_observation(&s, &failed),
        ProcessAssociation::Unavailable
    );
    // A same-named process whose command line the OS withheld is a candidate
    // we cannot rule out: never Unmatched.
    let hidden = snap(vec![proc(40, "codex", None, None)]);
    assert_eq!(
        associate(&s, &hidden),
        ProcessAssociation::Candidates { count: 1 }
    );
    // An unreadable runtime wrapper may hide an OpenCode/Claude node process.
    let wrapper = snap(vec![proc(41, "node", None, None)]);
    assert_eq!(associate(&s, &wrapper), ProcessAssociation::Unavailable);
    let ok = Ok(snap(vec![]));
    assert_eq!(
        association_from_observation(&s, &ok),
        ProcessAssociation::Unmatched
    );
}

#[test]
fn command_hint_is_sanitized_redacted_and_bounded() {
    let long = "x".repeat(5000);
    let p = proc(
        1,
        "codex",
        None,
        Some(&[
            "codex",
            "exec",
            "\u{1b}[31mhi\u{1b}[0m sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
            &long,
        ]),
    );
    let hint = p.command_line.clone().unwrap();
    assert!(!hint.contains('\u{1b}'));
    assert!(!hint.contains("AAAAAAAAAAAAAAAA"));
    assert!(hint.len() <= 160);
    let dbg = format!("{p:?}");
    assert!(!dbg.contains("xxxxxxxx") && !dbg.contains("AAAAAAAAAAAAAAAA"));
}

/// Held by the spawned copy below; runs only when explicitly requested.
#[test]
#[ignore]
fn hold_process_helper() {
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn observer_matches_spawned_binary_with_synthetic_session_id() {
    let id = "019d0000-1234-7abc-8def-0123456789ab";
    let dir = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    let copy = dir.path().join(name);
    std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    let mut child = Command::new(&copy)
        .args(["resume", id, "--ignored", "hold_process_helper"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let s = session(Provider::Codex, id, None);
    let other = session(
        Provider::Codex,
        "019d0000-ffff-7abc-8def-0123456789ab",
        None,
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut got, mut got_other) = (
        ProcessAssociation::NotChecked,
        ProcessAssociation::NotChecked,
    );
    while Instant::now() < deadline {
        let snapshot = observe_processes(0).expect("observe");
        got = associate(&s, &snapshot);
        got_other = associate(&other, &snapshot);
        if matches!(got, ProcessAssociation::Matched { .. }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    match got {
        ProcessAssociation::Matched { pid: p, .. } => assert_eq!(p, pid),
        other => panic!("expected match, got {other:?}"),
    }
    assert!(matches!(got_other, ProcessAssociation::Candidates { .. }));
}

#[test]
fn option_values_never_carry_identity_flags() {
    assert!(!matches(
        "claude",
        &["--system-prompt", "--resume=01a1", "-p", "hello"]
    ));
    assert!(!matches("claude", &["--system-prompt", "--resume", "01a1"]));
    assert!(!matches(
        "claude",
        &["--append-system-prompt", "-r", "01a1"]
    ));
    assert!(!matches("opencode", &["--prompt", "--session=ses_01a1"]));
    assert!(!matches("opencode", &["-m", "-s", "ses_01a1"]));
    // A real identity flag after an unrelated option's value still matches.
    assert!(matches("claude", &["--model", "opus", "--resume", "01a1"]));
    assert!(matches("opencode", &["--agent", "build", "-s", "ses_01a1"]));
}

#[test]
fn runtime_script_must_be_the_provider_program() {
    let id = "ses_01a1";
    assert!(!identity_matches(
        Provider::OpenCode,
        &["node", "/tmp/other.js", "opencode", "--session", id],
        id
    ));
    assert!(!identity_matches(
        Provider::OpenCode,
        &["node", "-e", "x", "opencode", "--session", id],
        id
    ));
    assert!(identity_matches(
        Provider::OpenCode,
        &[
            "node",
            "--require",
            "/x/hook.js",
            "/y/opencode",
            "--session",
            id
        ],
        id
    ));
    assert!(identity_matches(
        Provider::OpenCode,
        &["bun", "run", "/y/opencode", "-s", id],
        id
    ));
    // Such an invocation is still a visible candidate.
    let s = session(Provider::OpenCode, id, None);
    let snapshot = snap(vec![proc(
        9,
        "node",
        None,
        Some(&["node", "/tmp/other.js", "opencode", "--session", id]),
    )]);
    assert_eq!(
        associate(&s, &snapshot),
        ProcessAssociation::Candidates { count: 1 }
    );
}

#[test]
fn unknown_ownership_is_not_confirmed() {
    use agentscope::processes::ownership_confirmed;
    assert!(ownership_confirmed(Some(&1), Some(&1)));
    assert!(!ownership_confirmed(Some(&1), Some(&2)));
    assert!(!ownership_confirmed(None, Some(&1)));
    assert!(!ownership_confirmed(Some(&1), None));
    assert!(!ownership_confirmed::<u32>(None, None));
}

#[test]
fn windows_cwd_is_never_observed() {
    assert_eq!(agentscope::processes::CWD_OBSERVED, !cfg!(windows));
    if cfg!(windows) {
        let snapshot = observe_processes(0).unwrap();
        assert!(snapshot.processes.iter().all(|p| p.cwd.is_none()));
    }
}
