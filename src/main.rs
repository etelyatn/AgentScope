use agentscope::codex_reader::{CodexReader, Record};
use std::fs::File;
use std::io::{self, BufReader, Write};
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
            match File::open(path).and_then(|file| inspect(BufReader::new(file))) {
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

fn inspect(source: impl io::BufRead) -> io::Result<()> {
    let mut reader = CodexReader::new(source, MAX_RECORD_BYTES);
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    while let Some(record) = reader.next_record()? {
        match record {
            Record::Json { offset, value } => {
                let payload = &value["payload"];
                if value["type"] == "session_meta" {
                    if let Some(id) = payload["id"].as_str() {
                        writeln!(stdout, "Session: {}", safe_text(id))?;
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
                                writeln!(stdout, "[{role} @ {offset}] {}", safe_text(text))?;
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

// Escape controls and directional formatting; mask common credential-shaped tokens.
// Redaction is best effort and cannot identify every secret.
fn safe_text(text: &str) -> String {
    let mut output = String::new();
    let mut mask_next = false;
    let mut masked_quote = None;
    let mut quote_escape = false;
    for token in text.split_inclusive(char::is_whitespace) {
        if let Some(quote) = masked_quote {
            for ch in token.chars() {
                if quote_escape {
                    quote_escape = false;
                } else if ch == '\\' {
                    quote_escape = true;
                } else if ch == quote {
                    masked_quote = None;
                    break;
                }
            }
            continue;
        }
        let lower = token.to_ascii_lowercase();
        let mask = mask_next
            || ["sk-", "ghp_", "github_pat_", "xoxb-", "xoxp-"]
                .iter()
                .any(|prefix| lower.contains(prefix));
        let trimmed = lower.trim().trim_matches([':', '=', '"', '\'']);
        let key = ["api_key", "api-key", "password", "authorization"]
            .iter()
            .any(|key| lower.contains(key));
        let separator = trimmed.is_empty() && lower.contains([':', '=']);
        mask_next = trimmed == "bearer"
            || (key && !lower.contains('=') && !lower.contains(':'))
            || (key && lower.trim_end().ends_with([':', '=']))
            || (mask_next && separator);
        if (mask && !separator)
            || (key
                && (lower.contains('=') || lower.contains(':'))
                && !lower.trim_end().ends_with([':', '=']))
        {
            if mask && let Some(quote) = token.chars().next().filter(|ch| *ch == '"' || *ch == '\'')
            {
                masked_quote = Some(quote);
                quote_escape = false;
                for ch in token.chars().skip(1) {
                    if quote_escape {
                        quote_escape = false;
                    } else if ch == '\\' {
                        quote_escape = true;
                    } else if ch == quote {
                        masked_quote = None;
                        break;
                    }
                }
            }
            output.push_str("[redacted] ");
            continue;
        }
        for ch in token.chars() {
            if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", ch as u32);
            } else {
                output.push(ch);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::safe_text;

    #[test]
    fn masks_quoted_multiword_credentials_and_escapes_controls() {
        let text = r#"{"api_key": "synthetic secret words"} ok"#;
        let displayed = safe_text(text);
        assert!(!displayed.contains("synthetic"));
        assert!(!displayed.contains("secret"));
        assert!(!displayed.contains("words"));
        assert!(displayed.contains("ok"));
        assert!(!safe_text("\u{1b}]0;hello\u{7}\u{202e}").contains('\u{1b}'));
        assert!(!safe_text("\u{1b}]0;hello\u{7}\u{202e}").contains('\u{202e}'));
    }
}
