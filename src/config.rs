//! Configuration loading and the exact-match price schedule.
//!
//! Precedence: CLI flags > environment > config file > defaults. A missing
//! default config file is fine; a malformed or non-USD file is a fatal error.
//! Loading never mutates process-global state; `load_with_env` takes an explicit
//! environment so callers and tests stay hermetic.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::domain::Provider;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config file {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("unsupported currency {0:?}: only \"USD\" is supported")]
    Currency(String),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("duplicate price entry for provider {provider:?}, model {model:?}")]
    DuplicatePrice { provider: Provider, model: String },
}

/// Command-line overrides relevant to configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub config: Option<PathBuf>,
    /// Exact Codex sessions root.
    pub codex_sessions: Option<PathBuf>,
    /// Exact Claude projects root.
    pub claude_projects: Option<PathBuf>,
    pub opencode_db: Option<PathBuf>,
    pub no_processes: bool,
    pub no_color: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    pub root: Option<PathBuf>,
    pub db: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderConfigs {
    pub codex: ProviderConfig,
    pub claude: ProviderConfig,
    pub opencode: ProviderConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceEntry {
    pub provider: Provider,
    pub model: String,
    pub as_of: Option<String>,
    pub input_per_mtok: f64,
    pub cached_input_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
    pub cache_write_5m_per_mtok: Option<f64>,
    pub cache_write_1h_per_mtok: Option<f64>,
    pub cache_read_per_mtok: Option<f64>,
    pub output_per_mtok: f64,
    pub reasoning_per_mtok: Option<f64>,
}

/// Exact provider+model price entries. No wildcards, folding, or fallback.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PriceSchedule {
    entries: HashMap<(Provider, String), PriceEntry>,
}

impl PriceSchedule {
    /// Builds a schedule, rejecting invalid entries and duplicate keys.
    pub fn new(entries: Vec<PriceEntry>) -> Result<Self, ConfigError> {
        let mut map = HashMap::with_capacity(entries.len());
        for entry in entries {
            validate_price(&entry)?;
            let key = (entry.provider, entry.model.clone());
            if map.contains_key(&key) {
                return Err(ConfigError::DuplicatePrice {
                    provider: entry.provider,
                    model: entry.model,
                });
            }
            map.insert(key, entry);
        }
        Ok(Self { entries: map })
    }

    pub fn price_for(&self, provider: Provider, model: &str) -> Option<&PriceEntry> {
        // Allocation-free lookup is not possible with a tuple key of String;
        // the schedule is tiny and consulted per session, not per event.
        self.entries.get(&(provider, model.to_owned()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn validate_price(e: &PriceEntry) -> Result<(), ConfigError> {
    if e.model.trim().is_empty() {
        return Err(ConfigError::Invalid(
            "pricing entry has an empty model".into(),
        ));
    }
    let fields = [
        ("input_per_mtok", Some(e.input_per_mtok)),
        ("cached_input_per_mtok", e.cached_input_per_mtok),
        ("cache_write_per_mtok", e.cache_write_per_mtok),
        ("cache_write_5m_per_mtok", e.cache_write_5m_per_mtok),
        ("cache_write_1h_per_mtok", e.cache_write_1h_per_mtok),
        ("cache_read_per_mtok", e.cache_read_per_mtok),
        ("output_per_mtok", Some(e.output_per_mtok)),
        ("reasoning_per_mtok", e.reasoning_per_mtok),
    ];
    for (name, value) in fields {
        if let Some(v) = value
            && (!v.is_finite() || v < 0.0)
        {
            return Err(ConfigError::Invalid(format!(
                "pricing entry {:?}: {name} must be a finite non-negative number",
                e.model
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub refresh_ms: u64,
    pub process_ms: u64,
    pub process_enabled: bool,
    pub quiet_after_secs: u64,
    pub stall_after_secs: u64,
    pub max_sessions: usize,
    pub max_page_events: usize,
    pub max_page_bytes: usize,
    pub max_record_bytes: usize,
    pub dedup_ledger_entries: usize,
    pub dedup_ledger_bytes: usize,
    pub refresh_busy_timeout_ms: u64,
    pub page_busy_timeout_ms: u64,
    pub redaction: bool,
    pub providers: ProviderConfigs,
    pub pricing: PriceSchedule,
    /// Config file path that was requested or resolved by default, if any.
    pub config_path: Option<PathBuf>,
    pub no_color: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    currency: Option<String>,
    refresh_ms: Option<u64>,
    process_ms: Option<u64>,
    process_enabled: Option<bool>,
    quiet_after_secs: Option<u64>,
    stall_after_secs: Option<u64>,
    max_sessions: Option<usize>,
    max_page_events: Option<usize>,
    max_page_bytes: Option<usize>,
    max_record_bytes: Option<usize>,
    dedup_ledger_entries: Option<usize>,
    dedup_ledger_bytes: Option<usize>,
    refresh_busy_timeout_ms: Option<u64>,
    page_busy_timeout_ms: Option<u64>,
    redaction: Option<bool>,
    #[serde(default)]
    providers: RawProviders,
    #[serde(default)]
    pricing: Vec<RawPrice>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProviders {
    #[serde(default)]
    codex: RawRoot,
    #[serde(default)]
    claude: RawRoot,
    #[serde(default)]
    opencode: RawDb,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRoot {
    root: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDb {
    db: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrice {
    provider: String,
    model: String,
    as_of: Option<String>,
    input_per_mtok: f64,
    cached_input_per_mtok: Option<f64>,
    cache_write_per_mtok: Option<f64>,
    cache_write_5m_per_mtok: Option<f64>,
    cache_write_1h_per_mtok: Option<f64>,
    cache_read_per_mtok: Option<f64>,
    output_per_mtok: f64,
    reasoning_per_mtok: Option<f64>,
}

impl RawPrice {
    fn into_entry(self) -> Result<PriceEntry, ConfigError> {
        let provider = match self.provider.as_str() {
            "codex" => Provider::Codex,
            "claude" => Provider::Claude,
            "opencode" => Provider::OpenCode,
            other => {
                return Err(ConfigError::Invalid(format!(
                    "pricing entry has unknown provider {other:?} (expected codex, claude, or opencode)"
                )));
            }
        };
        Ok(PriceEntry {
            provider,
            model: self.model,
            as_of: self.as_of,
            input_per_mtok: self.input_per_mtok,
            cached_input_per_mtok: self.cached_input_per_mtok,
            cache_write_per_mtok: self.cache_write_per_mtok,
            cache_write_5m_per_mtok: self.cache_write_5m_per_mtok,
            cache_write_1h_per_mtok: self.cache_write_1h_per_mtok,
            cache_read_per_mtok: self.cache_read_per_mtok,
            output_per_mtok: self.output_per_mtok,
            reasoning_per_mtok: self.reasoning_per_mtok,
        })
    }
}

/// Environment snapshot. Values stay `OsString` so non-Unicode paths survive.
pub type Env = HashMap<OsString, OsString>;

fn env_path(env: &Env, key: &str) -> Option<PathBuf> {
    let value = if cfg!(windows) {
        // Windows environment names are case-insensitive.
        env.iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    } else {
        env.get(OsStr::new(key))
    };
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn home_dir(env: &Env) -> Option<PathBuf> {
    env_path(env, "HOME").or_else(|| env_path(env, "USERPROFILE"))
}

fn default_config_path(env: &Env) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env_path(env, "APPDATA")?
    } else if cfg!(target_os = "macos") {
        home_dir(env)?.join("Library").join("Application Support")
    } else {
        env_path(env, "XDG_CONFIG_HOME").or_else(|| home_dir(env).map(|h| h.join(".config")))?
    };
    Some(base.join("agentscope").join("config.toml"))
}

fn positive<T: Default + PartialEq>(name: &str, value: T) -> Result<T, ConfigError> {
    if value == T::default() {
        Err(ConfigError::Invalid(format!(
            "{name} must be greater than 0"
        )))
    } else {
        Ok(value)
    }
}

impl Config {
    /// Loads configuration using the process environment.
    pub fn load(cli: &CliOverrides) -> Result<Self, ConfigError> {
        let env: Env = std::env::vars_os().collect();
        Self::load_with_env(cli, &env)
    }

    /// Loads configuration with an explicit environment map.
    ///
    /// A missing default config file is fine; an explicit `--config` path that
    /// cannot be read is an error.
    pub fn load_with_env(cli: &CliOverrides, env: &Env) -> Result<Self, ConfigError> {
        let explicit = cli.config.is_some();
        let config_path = cli.config.clone().or_else(|| default_config_path(env));
        let raw = match &config_path {
            Some(path) => read_raw(path, explicit)?,
            None => RawConfig::default(),
        };

        if let Some(currency) = &raw.currency
            && currency != "USD"
        {
            return Err(ConfigError::Currency(currency.clone()));
        }

        let quiet = positive("quiet_after_secs", raw.quiet_after_secs.unwrap_or(300))?;
        let stall = positive("stall_after_secs", raw.stall_after_secs.unwrap_or(900))?;
        if quiet >= stall {
            return Err(ConfigError::Invalid(format!(
                "quiet_after_secs ({quiet}) must be less than stall_after_secs ({stall})"
            )));
        }

        let entries = raw
            .pricing
            .into_iter()
            .map(RawPrice::into_entry)
            .collect::<Result<Vec<_>, _>>()?;

        let home = home_dir(env);
        let codex_root = cli
            .codex_sessions
            .clone()
            .or_else(|| env_path(env, "CODEX_HOME").map(|h| h.join("sessions")))
            .or(raw.providers.codex.root)
            .or_else(|| home.as_ref().map(|h| h.join(".codex").join("sessions")));
        let claude_root = cli
            .claude_projects
            .clone()
            .or_else(|| env_path(env, "CLAUDE_CONFIG_DIR").map(|h| h.join("projects")))
            .or(raw.providers.claude.root)
            .or_else(|| home.as_ref().map(|h| h.join(".claude").join("projects")));
        let opencode_db = cli
            .opencode_db
            .clone()
            .or_else(|| {
                env_path(env, "XDG_DATA_HOME").map(|d| d.join("opencode").join("opencode.db"))
            })
            .or(raw.providers.opencode.db)
            .or_else(|| {
                home.as_ref().map(|h| {
                    h.join(".local")
                        .join("share")
                        .join("opencode")
                        .join("opencode.db")
                })
            });

        Ok(Config {
            refresh_ms: positive("refresh_ms", raw.refresh_ms.unwrap_or(2000))?,
            process_ms: positive("process_ms", raw.process_ms.unwrap_or(5000))?,
            process_enabled: !cli.no_processes && raw.process_enabled.unwrap_or(true),
            quiet_after_secs: quiet,
            stall_after_secs: stall,
            max_sessions: positive("max_sessions", raw.max_sessions.unwrap_or(2000))?,
            max_page_events: positive("max_page_events", raw.max_page_events.unwrap_or(500))?,
            max_page_bytes: positive("max_page_bytes", raw.max_page_bytes.unwrap_or(262_144))?,
            max_record_bytes: positive(
                "max_record_bytes",
                raw.max_record_bytes.unwrap_or(1_048_576),
            )?,
            dedup_ledger_entries: positive(
                "dedup_ledger_entries",
                raw.dedup_ledger_entries.unwrap_or(100_000),
            )?,
            dedup_ledger_bytes: positive(
                "dedup_ledger_bytes",
                raw.dedup_ledger_bytes.unwrap_or(8_388_608),
            )?,
            refresh_busy_timeout_ms: positive(
                "refresh_busy_timeout_ms",
                raw.refresh_busy_timeout_ms.unwrap_or(2000),
            )?,
            page_busy_timeout_ms: positive(
                "page_busy_timeout_ms",
                raw.page_busy_timeout_ms.unwrap_or(250),
            )?,
            redaction: raw.redaction.unwrap_or(true),
            providers: ProviderConfigs {
                codex: ProviderConfig {
                    root: codex_root,
                    db: None,
                },
                claude: ProviderConfig {
                    root: claude_root,
                    db: None,
                },
                opencode: ProviderConfig {
                    root: None,
                    db: opencode_db,
                },
            },
            pricing: PriceSchedule::new(entries)?,
            config_path,
            no_color: cli.no_color,
        })
    }
}

fn read_raw(path: &Path, explicit: bool) -> Result<RawConfig, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound && !explicit => return Ok(RawConfig::default()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}
