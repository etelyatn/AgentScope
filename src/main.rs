use agentscope::app::App;
use agentscope::config::{CliOverrides, Config};
use agentscope::domain::Thresholds;
use agentscope::ingest::jsonl::{JsonlCursor, Record};
use agentscope::ingest::text::display_safe;
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::ui::{self, RosterContext};
use agentscope::worker::WorkerHandle;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_RECORD_BYTES: usize = 1024 * 1024;

const USAGE: &str = "Usage: agentscope [OPTIONS]\n       agentscope inspect <codex.jsonl>";

const HELP: &str =
    "AgentScope: read-only monitor for local Codex, Claude Code, and OpenCode sessions.

Usage:
  agentscope [OPTIONS]
  agentscope inspect <codex.jsonl>   Print a Codex transcript (diagnostic, no TUI)

Options:
  --config <path>           Config file (default: platform config directory)
  --codex-sessions <dir>    Codex sessions root
  --claude-projects <dir>   Claude projects root
  --opencode-db <path>      OpenCode database
  --no-processes            Disable process observation
  --no-color                Disable color (NO_COLOR is also honored)
  --version                 Print version
  --help                    Print this help

Keys: j/k move, g/G first/last, s sort, S reverse, r refresh, q or Ctrl+c quit.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Inspect(PathBuf),
    Run(CliOverrides),
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let mut cli = CliOverrides::default();
    let mut first = true;
    while let Some(arg) = args.next() {
        if first && arg == "inspect" {
            return match (args.next(), args.next()) {
                (Some(path), None) => Ok(Command::Inspect(PathBuf::from(path))),
                _ => Err("inspect takes exactly one file".into()),
            };
        }
        first = false;
        let text = arg.to_string_lossy().into_owned();
        let (name, inline) = match text.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(OsString::from(v))),
            _ => (text.clone(), None),
        };
        let value = |args: &mut dyn Iterator<Item = OsString>| {
            inline
                .clone()
                .or_else(|| args.next())
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match name.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" => return Ok(Command::Version),
            "--config" => cli.config = Some(value(&mut args)?),
            "--codex-sessions" => cli.codex_sessions = Some(value(&mut args)?),
            "--claude-projects" => cli.claude_projects = Some(value(&mut args)?),
            "--opencode-db" => cli.opencode_db = Some(value(&mut args)?),
            "--no-processes" if inline.is_none() => cli.no_processes = true,
            "--no-color" if inline.is_none() => cli.no_color = true,
            _ => return Err(format!("unrecognized argument: {}", display_safe(&text))),
        }
    }
    Ok(Command::Run(cli))
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("agentscope {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Inspect(path)) => match inspect(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Could not inspect source: {:?}", error.kind());
                ExitCode::FAILURE
            }
        },
        Ok(Command::Run(cli)) => run_tui(&cli),
        Err(message) => {
            eprintln!("agentscope: {message}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_tui(cli: &CliOverrides) -> ExitCode {
    // Checked before any terminal mode change.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "agentscope: stdin and stdout must be a terminal for the interactive view.\n\
             To read a transcript without the TUI, use: agentscope inspect <codex.jsonl>"
        );
        return ExitCode::from(2);
    }
    let config = match Config::load(cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agentscope: {error}");
            return ExitCode::from(2);
        }
    };
    let no_color = config.no_color || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    let (readers, notes) = build_readers(&config);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut app = App::new(
        now,
        Thresholds {
            quiet: Duration::from_secs(config.quiet_after_secs),
            stall: Duration::from_secs(config.stall_after_secs),
        },
    );
    let mut ctx = RosterContext {
        no_color,
        roots: checked_roots(&config),
        notes,
        last_refresh: None,
        error: None,
    };
    let worker = WorkerHandle::spawn(readers, config.clone());
    let respawn = || WorkerHandle::spawn(build_readers(&config).0, config.clone());
    match ui::run(&mut app, &mut ctx, worker, &respawn) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agentscope: terminal error: {:?}", error.kind());
            ExitCode::FAILURE
        }
    }
}

fn build_readers(config: &Config) -> (Vec<Box<dyn ProviderReader>>, Vec<String>) {
    let mut readers: Vec<Box<dyn ProviderReader>> = Vec::new();
    let mut notes = Vec::new();
    let mut add = |name: &str, result: Result<Box<dyn ProviderReader>, _>| match result {
        Ok(reader) => readers.push(reader),
        Err(error) => notes.push(format!("{name} unavailable: {error}")),
    };
    add(
        "Codex",
        CodexScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    add(
        "Claude",
        ClaudeScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    add(
        "OpenCode",
        OpenCodeScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    (readers, notes)
}

fn checked_roots(config: &Config) -> Vec<String> {
    let show = |name: &str, path: &Option<PathBuf>| match path {
        Some(path) => format!("{name}: {}", path.display()),
        None => format!("{name}: no location configured"),
    };
    vec![
        show("Codex", &config.providers.codex.root),
        show("Claude", &config.providers.claude.root),
        show("OpenCode", &config.providers.opencode.db),
    ]
}

fn inspect(path: PathBuf) -> io::Result<()> {
    let mut reader = JsonlCursor::open(path, MAX_RECORD_BYTES)?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    while let Some(record) = reader.next_record()? {
        match record {
            Record::Json { offset, value } => {
                let payload = &value["payload"];
                if value["type"] == "session_meta" {
                    if let Some(id) = payload["id"].as_str() {
                        writeln!(stdout, "Session: {}", display_safe(id))?;
                    }
                } else if value["type"] == "response_item" && payload["type"] == "message" {
                    let role = match payload["role"].as_str() {
                        Some("user") => "User",
                        Some("assistant") => "Assistant",
                        _ => continue,
                    };
                    if let Some(parts) = payload["content"].as_array() {
                        for part in parts {
                            if (part["type"] == "input_text" || part["type"] == "output_text")
                                && let Some(text) = part["text"].as_str()
                            {
                                writeln!(stdout, "[{role} @ {offset}] {}", display_safe(text))?;
                            }
                        }
                    }
                }
            }
            Record::Invalid { offset } => writeln!(stderr, "Invalid JSON record at byte {offset}")?,
            Record::TooLarge { offset } => writeln!(stderr, "Oversized record at byte {offset}")?,
            Record::Incomplete { offset } => {
                writeln!(stderr, "Incomplete record at byte {offset}")?
            }
        }
    }
    Ok(())
}
