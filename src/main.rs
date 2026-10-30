use agentscope::ingest::jsonl::{JsonlCursor, Record};
use agentscope::ingest::text::display_safe;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const MAX_RECORD_BYTES: usize = 1024 * 1024;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next(), args.next()) {
        (None, None, None) => {
            println!("AgentScope is under development. Use: agentscope inspect <jsonl-file>");
            ExitCode::SUCCESS
        }
        (Some(command), Some(path), None) if command == "inspect" => {
            match inspect(PathBuf::from(path)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Could not inspect source: {:?}", error.kind());
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("Usage: agentscope inspect <jsonl-file>");
            ExitCode::FAILURE
        }
    }
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
