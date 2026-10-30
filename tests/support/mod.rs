//! Synthetic fixture builders shared by integration tests.
#![allow(dead_code)]

pub mod codex;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentscope::config::{CliOverrides, Config, Env};

/// Fixed clock for every test (2026-10-30T00:00:00Z in epoch milliseconds).
pub const NOW: i64 = 1_793_318_400_000;

/// Config whose Codex root is exactly `root`.
pub fn config_for(root: &Path) -> Config {
    let cli = CliOverrides {
        codex_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    Config::load_with_env(&cli, &Env::new()).expect("valid test config")
}

pub fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).expect("write fixture");
}

pub fn set_modified(path: &Path, secs_after_epoch: u64) {
    let file = File::options().write(true).open(path).expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
        .expect("set mtime");
}

pub fn dated(root: &Path, name: &str) -> PathBuf {
    root.join("2026").join("10").join("29").join(name)
}
