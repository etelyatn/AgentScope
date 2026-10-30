//! Read-only process observation and identity-argument matching (spec §9.4).
//!
//! Only OS-exposed process metadata is read: PID, start time, executable, cwd
//! when the OS provides it, and the argument vector used for identity matching.
//! Environments and process memory are never read and no process is signalled.
//! Executable names, working directories and timing never produce a match; they
//! only make a process a *candidate*.

use std::fmt;
use std::path::PathBuf;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use thiserror::Error;

use crate::domain::{ProcessAssociation, Provider, SessionSummary, Timestamp};
use crate::ingest::text;

const HINT_MAX_BYTES: usize = 160;
const HINT_MAX_COLS: usize = 120;
/// Runtimes that may host a provider CLI script (`node …/opencode`).
const RUNTIMES: [&str; 3] = ["node", "bun", "deno"];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProcessError {
    /// The OS did not expose a process list (unsupported platform or denied).
    #[error("process list unavailable")]
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct ProcessSnapshot {
    pub sampled_at: Timestamp,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub started_at: Option<Timestamp>,
    /// Display-safe executable path (or process name when the path is hidden).
    pub executable: String,
    /// `None` when the OS does not expose it (generally on Windows).
    pub cwd: Option<PathBuf>,
    /// Sanitized, redacted, truncated hint; never the raw command line.
    pub command_line: Option<String>,
    /// Raw arguments, used only for matching. Never displayed or logged.
    argv: Option<Vec<String>>,
}

impl ProcessInfo {
    pub fn new(
        pid: u32,
        started_at: Option<Timestamp>,
        executable: String,
        cwd: Option<PathBuf>,
        argv: Option<Vec<String>>,
    ) -> Self {
        let command_line = argv.as_deref().filter(|a| !a.is_empty()).map(hint);
        Self {
            pid,
            started_at,
            executable: text::display_safe(&executable),
            cwd,
            command_line,
            argv: argv.filter(|a| !a.is_empty()),
        }
    }
}

impl fmt::Debug for ProcessInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessInfo")
            .field("pid", &self.pid)
            .field("started_at", &self.started_at)
            .field("executable_len", &self.executable.len())
            .field("cwd_known", &self.cwd.is_some())
            .field("argc", &self.argv.as_ref().map(Vec::len))
            .finish()
    }
}

fn hint(argv: &[String]) -> String {
    let mut parts = Vec::with_capacity(argv.len());
    for (i, a) in argv.iter().enumerate() {
        // Executable path may embed a user name; show the basename only.
        parts.push(if i == 0 {
            basename(a).to_string()
        } else {
            a.clone()
        });
    }
    text::preview(&parts.join(" "), HINT_MAX_BYTES, HINT_MAX_COLS).preview
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Lower-cased basename without a launcher/script extension.
fn stem(path: &str) -> String {
    let base = basename(path).to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".js", ".mjs", ".cjs"] {
        if let Some(s) = base.strip_suffix(ext) {
            return s.to_string();
        }
    }
    base
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
        Provider::OpenCode => "opencode",
    }
}

/// Index of the provider program in `argv`: argv[0], or argv[1..3] when argv[0]
/// is a script runtime (`node /path/opencode …`).
fn locate(provider: Provider, argv: &[&str]) -> Option<usize> {
    let name = provider_name(provider);
    if stem(argv.first()?) == name {
        return Some(0);
    }
    if RUNTIMES.contains(&stem(argv[0]).as_str()) {
        return (1..argv.len().min(3)).find(|&i| stem(argv[i]) == name);
    }
    None
}

/// True when `argv` (including argv[0]) names the provider program and carries
/// `id` in an identity-bearing position. Prompt text and other subcommands never match.
pub fn identity_matches(provider: Provider, argv: &[&str], id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let Some(at) = locate(provider, argv) else {
        return false;
    };
    let args = &argv[at + 1..];
    match provider {
        Provider::Codex => match args {
            ["resume", got, ..] => *got == id,
            ["exec", "resume", got, ..] => *got == id,
            _ => false,
        },
        Provider::Claude => {
            flag_value(args, &["-r", "--resume"], "--resume=", "--fork-session", id)
        }
        Provider::OpenCode => flag_value(args, &["-s", "--session"], "--session=", "--fork", id),
    }
}

/// Scan flag arguments for `flag <id>` / `prefix<id>`. A fork flag makes the
/// id a parent reference, not the running session. `--` ends option parsing.
fn flag_value(args: &[&str], flags: &[&str], prefix: &str, fork: &str, id: &str) -> bool {
    let mut found = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            break;
        }
        if a == fork {
            return false;
        }
        if flags.contains(&a) {
            if args.get(i + 1) == Some(&id) {
                found = true;
            }
            i += 1;
        } else if a.strip_prefix(prefix) == Some(id) {
            found = true;
        }
        i += 1;
    }
    found
}

fn argv_refs(p: &ProcessInfo) -> Option<Vec<&str>> {
    p.argv
        .as_ref()
        .map(|a| a.iter().map(String::as_str).collect())
}

fn is_candidate(provider: Provider, p: &ProcessInfo) -> bool {
    if stem(&p.executable) == provider_name(provider) {
        return true;
    }
    argv_refs(p).is_some_and(|a| locate(provider, &a).is_some())
}

/// Processes that look like the provider's CLI. Candidates are not matches.
pub fn candidates(provider: Provider, snapshot: &ProcessSnapshot) -> Vec<&ProcessInfo> {
    snapshot
        .processes
        .iter()
        .filter(|p| is_candidate(provider, p))
        .collect()
}

/// Sample the OS process list. Environments and memory are never requested.
pub fn observe_processes(now: Timestamp) -> Result<ProcessSnapshot, ProcessError> {
    let kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .with_cwd(UpdateKind::Always)
        .with_user(UpdateKind::Always);
    let mut sys = System::new().map_err(|_| ProcessError::Unavailable)?;
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let own_user = sysinfo::get_current_pid()
        .ok()
        .and_then(|pid| sys.process(pid))
        .and_then(|p| p.user_id().cloned());
    let mut processes: Vec<ProcessInfo> = sys
        .processes()
        .values()
        .map(|p| {
            // Another user's command line and cwd are never surfaced.
            let foreign = match (&own_user, p.user_id()) {
                (Some(me), Some(them)) => me != them,
                _ => false,
            };
            let argv: Vec<String> = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let executable = p
                .exe()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
            let started = p.start_time();
            ProcessInfo::new(
                pid_u32(p.pid()),
                (started > 0).then(|| (started as i64).saturating_mul(1000)),
                executable,
                if foreign {
                    None
                } else {
                    p.cwd().map(PathBuf::from)
                },
                if foreign || argv.is_empty() {
                    None
                } else {
                    Some(argv)
                },
            )
        })
        .collect();
    if processes.is_empty() {
        return Err(ProcessError::Unavailable);
    }
    processes.sort_by_key(|p| p.pid);
    Ok(ProcessSnapshot {
        sampled_at: now,
        processes,
    })
}

fn pid_u32(pid: Pid) -> u32 {
    pid.as_u32()
}

/// Map an observation attempt to an association; failure is `Unavailable`,
/// never `Unmatched`.
pub fn association_from_observation(
    session: &SessionSummary,
    observation: &Result<ProcessSnapshot, ProcessError>,
) -> ProcessAssociation {
    match observation {
        Ok(snapshot) => associate(session, snapshot),
        Err(_) => ProcessAssociation::Unavailable,
    }
}

/// Strong match only through an identity-bearing argument. Otherwise
/// same-provider processes are counted as candidates; if the OS withheld the
/// arguments of a runtime that could be hosting the provider, the answer is
/// `Unavailable` rather than a false `Unmatched`.
pub fn associate(session: &SessionSummary, snapshot: &ProcessSnapshot) -> ProcessAssociation {
    let provider = session.key.provider;
    let id = session.key.provider_session_id.as_str();
    let matched = snapshot
        .processes
        .iter()
        .filter(|p| argv_refs(p).is_some_and(|a| identity_matches(provider, &a, id)))
        .min_by_key(|p| (p.started_at.unwrap_or(Timestamp::MAX), p.pid));
    if let Some(p) = matched {
        return ProcessAssociation::Matched {
            pid: p.pid,
            started_at: p.started_at,
        };
    }
    let count = snapshot
        .processes
        .iter()
        .filter(|p| is_candidate(provider, p))
        .count();
    if count > 0 {
        return ProcessAssociation::Candidates { count };
    }
    let hidden_runtime = snapshot
        .processes
        .iter()
        .any(|p| p.argv.is_none() && RUNTIMES.contains(&stem(&p.executable).as_str()));
    if hidden_runtime {
        ProcessAssociation::Unavailable
    } else {
        ProcessAssociation::Unmatched
    }
}
