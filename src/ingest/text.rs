//! Display safety for untrusted text: terminal-control sanitization,
//! best-effort credential redaction, and bounded previews.
//!
//! Redaction is best-effort masking of common credential shapes. It is not a
//! guarantee that no secret is shown, and it may mask innocuous text.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::BoundedText;

const TAB_STOP: usize = 4;
const MASK: &str = "[REDACTED]";

/// Strip terminal escape sequences, control characters, and invisible
/// directional formatting; expand tabs and normalize line endings to `\n`.
///
/// Invalid UTF-8 is already decoded to U+FFFD by callers (`String::from_utf8_lossy`);
/// that visible marker is preserved.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut col = 0usize;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    skip_csi(&mut chars);
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    skip_string(&mut chars);
                }
                Some(n) if ('\u{20}'..='\u{2f}').contains(&n) => {
                    // nF escape: intermediates then one final byte.
                    while chars
                        .next_if(|n| ('\u{20}'..='\u{2f}').contains(n))
                        .is_some()
                    {}
                    chars.next_if(|n| ('\u{30}'..='\u{7e}').contains(n));
                }
                Some(n) if ('\u{30}'..='\u{7e}').contains(&n) => {
                    chars.next();
                }
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
                col = 0;
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = TAB_STOP - col % TAB_STOP;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            c if c.is_control() || is_invisible_format(c) => {}
            c => {
                out.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    out
}

/// Skip the remainder of a CSI sequence (after `ESC [`).
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars
        .next_if(|c| ('\u{30}'..='\u{3f}').contains(c))
        .is_some()
    {}
    while chars
        .next_if(|c| ('\u{20}'..='\u{2f}').contains(c))
        .is_some()
    {}
    chars.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
}

/// Skip an OSC/DCS/SOS/PM/APC string body. Terminated by BEL, ST (`ESC \` or
/// U+009C). An unterminated string ends at the next newline so one malformed
/// sequence cannot swallow the rest of the text.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        match c {
            '\u{07}' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\u{1b}' => {
                chars.next();
                chars.next_if_eq(&'\\');
                return;
            }
            '\n' | '\r' => return,
            _ => {
                chars.next();
            }
        }
    }
}

fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'                 // Arabic letter mark
        | '\u{180e}'               // Mongolian vowel separator
        | '\u{200b}'..='\u{200f}'  // zero-width space/joiners, LRM, RLM
        | '\u{2028}'..='\u{202e}'  // line/paragraph sep, bidi embeddings/overrides
        | '\u{2060}'..='\u{206f}'  // word joiner, invisible operators, bidi isolates
        | '\u{feff}'               // BOM / zero-width no-break space
    )
}

static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)")
        .expect("valid regex")
});

static TOKEN_SHAPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\bsk-[A-Za-z0-9_-]{8,}",
        r"|\bgh[pousr]_[A-Za-z0-9]{20,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{20,}",
        r"|\bxox[abprs]-[A-Za-z0-9-]{8,}",
        r"|\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    ))
    .expect("valid regex")
});

static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\bbearer\s+)[A-Za-z0-9._~+/=-]{8,}").expect("valid regex"));

// Key must END with the credential word (so `max_tokens` is not matched).
// Unquoted values must not start with an operator/bracket (`==`, `(`, `&`, ...).
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)(\b[\w-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key)["']?\s*[:=]\s*)"#,
        r#"("(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|[^\s,;"'=(){}\[\]<>&$|][^\s,;"'&|]*)"#,
    ))
    .expect("valid regex")
});

// Command-line style `--password value` / `--api-key value` (whitespace-separated).
static CLI_ARG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(--?[\w-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key)\s+)",
        r#"("(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|[^\s"'=:-][^\s]*)"#,
    ))
    .expect("valid regex")
});

/// Best-effort masking of common credential shapes. Not a guarantee.
pub fn redact(input: &str) -> String {
    let s = PRIVATE_KEY.replace_all(input, MASK);
    let s = TOKEN_SHAPES.replace_all(&s, MASK);
    let s = BEARER.replace_all(&s, |c: &Captures<'_>| format!("{}{MASK}", &c[1]));
    let s = CLI_ARG.replace_all(&s, |c: &Captures<'_>| format!("{}{MASK}", &c[1]));
    ASSIGNMENT
        .replace_all(&s, |c: &Captures<'_>| {
            let value = &c[2];
            let quoted = value.starts_with(['"', '\'']);
            // Unquoted values that look like expressions (calls) are code, not secrets.
            if !quoted && (value.contains('(') || value.contains(')')) {
                return c[0].to_string();
            }
            format!("{}{MASK}", &c[1])
        })
        .into_owned()
}

/// Sanitize, then redact. Sanitizing first stops escape/zero-width characters
/// from splitting a secret out of redaction's reach.
pub fn display_safe(input: &str) -> String {
    redact(&sanitize(input))
}

/// Build a bounded, display-safe preview. Truncation is applied after
/// sanitization and redaction, so a cut can never expose part of a secret.
/// `byte_len` is the original input length. Columns use Unicode display width
/// (newlines count as one column). The locator is left `None` for the caller.
pub fn preview(input: &str, max_bytes: usize, max_cols: usize) -> BoundedText {
    let safe = display_safe(input);
    let mut end = 0usize;
    let mut base = 0usize; // columns consumed before the current line segment
    let mut seg_start = 0usize;
    for (i, c) in safe.char_indices() {
        let next = i + c.len_utf8();
        if next > max_bytes {
            break;
        }
        // Width of a sequence (e.g. heart + VS16) is not the sum of its chars,
        // so measure the retained line segment as a string after each char.
        let seg_cols = safe[seg_start..i].width();
        let total = if c == '\n' {
            base + seg_cols + 1
        } else {
            base + safe[seg_start..next].width()
        };
        if total > max_cols {
            break;
        }
        if c == '\n' {
            base = total;
            seg_start = next;
        }
        end = next;
    }
    BoundedText {
        truncated: end < safe.len(),
        preview: safe[..end].to_string(),
        byte_len: input.len(),
        locator: None,
    }
}
