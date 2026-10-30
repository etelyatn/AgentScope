//! Read-only discovery of provider session files (spec §7.1).
//!
//! Only the configured session root is walked. Symlinks are never followed, and
//! only files matching the provider's session-file pattern are returned, so
//! credential files and unrelated provider data are unreachable by construction.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::domain::{Provider, Warning, WarningKey, WarningKind};
use crate::providers::ProviderError;

/// Rollout files live at `<root>/YYYY/MM/DD/`; leave slack for layout changes.
const MAX_DEPTH: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    pub sources: Vec<SourceFile>,
    pub warnings: Vec<Warning>,
}

fn warning(source: String, kind: WarningKind, message: String, count: u32) -> Warning {
    Warning {
        key: WarningKey {
            provider: Provider::Codex,
            source,
            kind,
        },
        message,
        count,
    }
}

fn is_rollout(name: &str) -> bool {
    name.len() > "rollout-.jsonl".len() && name.starts_with("rollout-") && name.ends_with(".jsonl")
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<SourceFile>, warnings: &mut Vec<Warning>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(warning(
                dir.display().to_string(),
                WarningKind::Io,
                format!("directory unreadable ({:?})", error.kind()),
                1,
            ));
            return;
        }
    };
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow symlinks, so links report as neither.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(&path, depth + 1, out, warnings);
            }
        } else if kind.is_file() && is_rollout(&entry.file_name().to_string_lossy()) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            out.push(SourceFile {
                path,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: meta.len(),
            });
        }
    }
}

/// Lists Codex rollout files, newest first by modified time (ties by path), capped
/// at `config.max_sessions`. A missing root is a warning, not an error.
pub fn discover_codex(config: &Config) -> Result<Discovery, ProviderError> {
    let mut warnings = Vec::new();
    let Some(root) = config.providers.codex.root.as_deref() else {
        warnings.push(warning(
            "codex".into(),
            WarningKind::Unavailable,
            "no Codex sessions root could be determined".into(),
            1,
        ));
        return Ok(Discovery {
            sources: Vec::new(),
            warnings,
        });
    };
    // The root is an explicit user choice; only entries beneath it are never followed.
    if !fs::metadata(root).is_ok_and(|m| m.is_dir()) {
        warnings.push(warning(
            root.display().to_string(),
            WarningKind::Unavailable,
            "Codex sessions root is missing or not a directory".into(),
            1,
        ));
        return Ok(Discovery {
            sources: Vec::new(),
            warnings,
        });
    }
    let mut sources = Vec::new();
    walk(root, 0, &mut sources, &mut warnings);
    sources.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    if sources.len() > config.max_sessions {
        let omitted = sources.len() - config.max_sessions;
        sources.truncate(config.max_sessions);
        warnings.push(warning(
            root.display().to_string(),
            WarningKind::Unsupported,
            format!("{omitted} older Codex sessions omitted by the session cap"),
            u32::try_from(omitted).unwrap_or(u32::MAX),
        ));
    }
    Ok(Discovery { sources, warnings })
}
