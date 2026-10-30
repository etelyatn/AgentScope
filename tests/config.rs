use std::ffi::OsString;
use std::path::PathBuf;

use agentscope::config::{CliOverrides, Config, ConfigError, Env};
use agentscope::domain::Provider;
use tempfile::TempDir;

fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

/// Writes `toml` to a temp file and loads it with an explicit env map.
fn try_load(cli: CliOverrides, env: &Env, toml: &str) -> Result<Config, ConfigError> {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).unwrap();
    let cli = CliOverrides {
        config: Some(path),
        ..cli
    };
    Config::load_with_env(&cli, env)
}

fn load_from(toml: &str) -> Result<Config, ConfigError> {
    try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), toml)
}

fn ok(toml: &str) -> Config {
    load_from(toml).unwrap()
}

#[test]
fn defaults_match_spec() {
    let cfg = ok("");
    assert_eq!(cfg.refresh_ms, 2000);
    assert_eq!(cfg.process_ms, 5000);
    assert!(cfg.process_enabled);
    assert_eq!(cfg.quiet_after_secs, 300);
    assert_eq!(cfg.stall_after_secs, 900);
    assert_eq!(cfg.max_sessions, 2000);
    assert_eq!(cfg.max_page_events, 500);
    assert_eq!(cfg.max_page_bytes, 262_144);
    assert_eq!(cfg.max_record_bytes, 1_048_576);
    assert_eq!(cfg.dedup_ledger_entries, 100_000);
    assert_eq!(cfg.dedup_ledger_bytes, 8_388_608);
    assert_eq!(cfg.refresh_busy_timeout_ms, 2000);
    assert_eq!(cfg.page_busy_timeout_ms, 250);
    assert!(cfg.redaction);
    assert!(cfg.pricing.price_for(Provider::Codex, "x").is_none());
}

#[test]
fn default_provider_paths_come_from_home_and_xdg() {
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), "").unwrap();
    assert_eq!(
        cfg.providers.codex.root,
        Some(PathBuf::from("/home/u").join(".codex").join("sessions"))
    );
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from("/home/u").join(".claude").join("projects"))
    );
    assert_eq!(
        cfg.providers.opencode.db,
        Some(
            PathBuf::from("/home/u")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db")
        )
    );

    let cfg = try_load(
        CliOverrides::default(),
        &env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/xdg")]),
        "",
    )
    .unwrap();
    assert_eq!(
        cfg.providers.opencode.db,
        Some(PathBuf::from("/xdg").join("opencode").join("opencode.db"))
    );
}

#[test]
fn file_values_override_defaults() {
    let cfg =
        ok("refresh_ms = 100\nredaction = false\nprocess_enabled = false\nmax_sessions = 7\n");
    assert_eq!(cfg.refresh_ms, 100);
    assert!(!cfg.redaction);
    assert!(!cfg.process_enabled);
    assert_eq!(cfg.max_sessions, 7);
}

#[test]
fn malformed_non_usd_and_bad_thresholds_are_fatal() {
    assert!(matches!(
        load_from("currency = \"EUR\""),
        Err(ConfigError::Currency(_))
    ));
    assert!(load_from("currency = \"USD\"").is_ok());
    assert!(load_from("quiet_after_secs = 900\nstall_after_secs = 300").is_err());
    assert!(load_from("quiet_after_secs = 300\nstall_after_secs = 300").is_err());
    assert!(matches!(
        load_from("this is not toml"),
        Err(ConfigError::Parse { .. })
    ));
    assert!(load_from("refresh_mss = 5").is_err());
}

#[test]
fn zero_intervals_and_caps_are_rejected() {
    for key in [
        "refresh_ms",
        "process_ms",
        "quiet_after_secs",
        "max_sessions",
        "max_page_events",
        "max_page_bytes",
        "max_record_bytes",
        "dedup_ledger_entries",
        "dedup_ledger_bytes",
        "refresh_busy_timeout_ms",
        "page_busy_timeout_ms",
    ] {
        assert!(load_from(&format!("{key} = 0")).is_err(), "{key}");
    }
    assert!(load_from("stall_after_secs = 0").is_err());
}

#[test]
fn cli_beats_env_beats_file_beats_defaults() {
    let file = "[providers.codex]\nroot = \"/file\"\n[providers.claude]\nroot = \"/file-claude\"\n[providers.opencode]\ndb = \"/file.db\"\n";

    // File beats defaults.
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), file).unwrap();
    assert_eq!(cfg.providers.codex.root, Some(PathBuf::from("/file")));
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from("/file-claude"))
    );
    assert_eq!(cfg.providers.opencode.db, Some(PathBuf::from("/file.db")));

    // Env beats file; homes get a suffix appended.
    let e = env(&[
        ("HOME", "/h"),
        ("CODEX_HOME", "/env-codex"),
        ("CLAUDE_CONFIG_DIR", "/env-claude"),
        ("XDG_DATA_HOME", "/xdg"),
    ]);
    let cfg = try_load(CliOverrides::default(), &e, file).unwrap();
    assert_eq!(
        cfg.providers.codex.root,
        Some(PathBuf::from("/env-codex").join("sessions"))
    );
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from("/env-claude").join("projects"))
    );
    assert_eq!(
        cfg.providers.opencode.db,
        Some(PathBuf::from("/xdg").join("opencode").join("opencode.db"))
    );

    // CLI beats env, and is an exact root (no suffix).
    let cli = CliOverrides {
        codex_sessions: Some(PathBuf::from("/cli")),
        claude_projects: Some(PathBuf::from("/cli-claude")),
        opencode_db: Some(PathBuf::from("/cli.db")),
        ..CliOverrides::default()
    };
    let cfg = try_load(cli, &e, file).unwrap();
    assert_eq!(cfg.providers.codex.root, Some(PathBuf::from("/cli")));
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from("/cli-claude"))
    );
    assert_eq!(cfg.providers.opencode.db, Some(PathBuf::from("/cli.db")));
}

#[test]
fn empty_env_values_are_ignored() {
    let e = env(&[("HOME", "/h"), ("CODEX_HOME", ""), ("XDG_DATA_HOME", "")]);
    let cfg = try_load(
        CliOverrides::default(),
        &e,
        "[providers.codex]\nroot = \"/file\"\n",
    )
    .unwrap();
    assert_eq!(cfg.providers.codex.root, Some(PathBuf::from("/file")));
    assert_eq!(
        cfg.providers.opencode.db,
        Some(
            PathBuf::from("/h")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db")
        )
    );
}

#[test]
fn no_processes_no_color_and_config_path_are_preserved() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("c.toml");
    std::fs::write(&path, "process_enabled = true\n").unwrap();
    let cli = CliOverrides {
        config: Some(path.clone()),
        no_processes: true,
        no_color: true,
        ..CliOverrides::default()
    };
    let cfg = Config::load_with_env(&cli, &env(&[("HOME", "/h")])).unwrap();
    assert!(!cfg.process_enabled, "--no-processes beats file");
    assert_eq!(cfg.config_path, Some(path));
    assert!(cfg.no_color);

    let cfg = ok("");
    assert!(cfg.process_enabled);
    assert!(!cfg.no_color);
}

#[test]
fn missing_default_config_is_fine_but_explicit_missing_is_an_error() {
    let dir = TempDir::new().unwrap();
    let e = env(&[
        ("HOME", dir.path().to_str().unwrap()),
        ("XDG_CONFIG_HOME", dir.path().to_str().unwrap()),
        ("APPDATA", dir.path().to_str().unwrap()),
    ]);
    let cfg = Config::load_with_env(&CliOverrides::default(), &e).unwrap();
    assert_eq!(cfg.refresh_ms, 2000);

    let cli = CliOverrides {
        config: Some(dir.path().join("nope.toml")),
        ..CliOverrides::default()
    };
    assert!(matches!(
        Config::load_with_env(&cli, &e),
        Err(ConfigError::Io { .. })
    ));
}

#[test]
fn default_config_path_is_loaded_from_platform_location() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let e = env(&[("HOME", root), ("XDG_CONFIG_HOME", root), ("APPDATA", root)]);
    let cfg_dir = if cfg!(target_os = "macos") {
        dir.path().join("Library").join("Application Support")
    } else {
        dir.path().to_path_buf()
    }
    .join("agentscope");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "refresh_ms = 123\n").unwrap();
    let cfg = Config::load_with_env(&CliOverrides::default(), &e).unwrap();
    assert_eq!(cfg.refresh_ms, 123);
}

const PRICES: &str = r#"
[[pricing]]
provider = "codex"
model = "gpt-5.1-codex"
as_of = "2026-10-01"
input_per_mtok = 1.25
cached_input_per_mtok = 0.125
cache_write_per_mtok = 1.25
output_per_mtok = 10.0

[[pricing]]
provider = "claude"
model = "claude-sonnet-5-5"
input_per_mtok = 3.0
cache_write_5m_per_mtok = 3.75
cache_write_1h_per_mtok = 6.0
cache_read_per_mtok = 0.30
output_per_mtok = 15.0

[[pricing]]
provider = "opencode"
model = "gpt-5.1-codex"
input_per_mtok = 1.0
output_per_mtok = 2.0
reasoning_per_mtok = 3.0
"#;

#[test]
fn price_lookup_is_exact_and_duplicate_keys_are_rejected() {
    let cfg = ok(PRICES);
    let p = cfg
        .pricing
        .price_for(Provider::Codex, "gpt-5.1-codex")
        .unwrap();
    assert_eq!(p.input_per_mtok, 1.25);
    assert_eq!(p.cached_input_per_mtok, Some(0.125));
    assert_eq!(p.cache_write_per_mtok, Some(1.25));
    assert_eq!(p.as_of.as_deref(), Some("2026-10-01"));
    assert_eq!(p.reasoning_per_mtok, None);

    let c = cfg
        .pricing
        .price_for(Provider::Claude, "claude-sonnet-5-5")
        .unwrap();
    assert_eq!(c.cache_write_5m_per_mtok, Some(3.75));
    assert_eq!(c.cache_write_1h_per_mtok, Some(6.0));
    assert_eq!(c.cache_read_per_mtok, Some(0.30));
    assert_eq!(c.cached_input_per_mtok, None);

    // Same model under another provider is a distinct entry.
    let o = cfg
        .pricing
        .price_for(Provider::OpenCode, "gpt-5.1-codex")
        .unwrap();
    assert_eq!(o.reasoning_per_mtok, Some(3.0));

    // Exact: no case folding, prefixes, wildcards, or cross-provider fallback.
    assert!(
        cfg.pricing
            .price_for(Provider::Codex, "GPT-5.1-CODEX")
            .is_none()
    );
    assert!(cfg.pricing.price_for(Provider::Codex, "gpt-5.1").is_none());
    assert!(
        cfg.pricing
            .price_for(Provider::Codex, "gpt-5.1-codex ")
            .is_none()
    );
    assert!(
        cfg.pricing
            .price_for(Provider::Claude, "gpt-5.1-codex")
            .is_none()
    );

    let dup = format!(
        "{PRICES}\n[[pricing]]\nprovider = \"codex\"\nmodel = \"gpt-5.1-codex\"\ninput_per_mtok = 9.0\noutput_per_mtok = 9.0\n"
    );
    assert!(matches!(
        load_from(&dup),
        Err(ConfigError::DuplicatePrice { .. })
    ));
}

#[test]
fn invalid_price_entries_are_rejected() {
    let bad = [
        "provider = \"codex\"\nmodel = \"\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "provider = \"codex\"\nmodel = \"  \"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "provider = \"gemini\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "provider = \"codex\"\nmodel = \"m\"\ninput_per_mtok = -1.0\noutput_per_mtok = 1.0",
        "provider = \"codex\"\nmodel = \"m\"\ninput_per_mtok = 1.0",
        "provider = \"codex\"\nmodel = \"m\"\ninput_per_mtok = nan\noutput_per_mtok = 1.0",
        "provider = \"codex\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\nbogus = 1",
    ];
    for entry in bad {
        let toml = format!("[[pricing]]\n{entry}\n");
        assert!(load_from(&toml).is_err(), "{entry}");
    }
}

#[cfg(windows)]
#[test]
fn windows_env_names_are_case_insensitive() {
    let e = env(&[
        ("USERPROFILE", "C:\\h"),
        ("codex_home", "C:\\cx"),
        ("Claude_Config_Dir", "C:\\cl"),
        ("xdg_data_home", "C:\\x"),
    ]);
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.providers.codex.root,
        Some(PathBuf::from("C:\\cx").join("sessions"))
    );
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from("C:\\cl").join("projects"))
    );
    assert_eq!(
        cfg.providers.opencode.db,
        Some(PathBuf::from("C:\\x").join("opencode").join("opencode.db"))
    );
}

#[cfg(unix)]
#[test]
fn unix_env_names_are_case_sensitive() {
    let e = env(&[("HOME", "/h"), ("codex_home", "/wrong")]);
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.providers.codex.root,
        Some(PathBuf::from("/h").join(".codex").join("sessions"))
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_env_paths_still_win() {
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(b"/tmp/\xff\xfe".to_vec());
    let mut e = env(&[("HOME", "/h")]);
    e.insert("CODEX_HOME".into(), bad.clone());
    e.insert("CLAUDE_CONFIG_DIR".into(), bad.clone());
    e.insert("XDG_DATA_HOME".into(), bad.clone());
    let cfg = try_load(
        CliOverrides::default(),
        &e,
        "[providers.codex]\nroot = \"/file\"\n",
    )
    .unwrap();
    assert_eq!(
        cfg.providers.codex.root,
        Some(PathBuf::from(&bad).join("sessions"))
    );
    assert_eq!(
        cfg.providers.claude.root,
        Some(PathBuf::from(&bad).join("projects"))
    );
    assert_eq!(
        cfg.providers.opencode.db,
        Some(PathBuf::from(&bad).join("opencode").join("opencode.db"))
    );
}
