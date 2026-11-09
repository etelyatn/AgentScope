//! Markdown presentation for recorded agent replies.
//!
//! Input must already be display-safe (sanitized and redacted): this module
//! only rearranges and styles that text, it never adds content of its own
//! beyond list markers, quote bars, rules and link targets taken from the same
//! text. Raw HTML is shown literally, never interpreted. Output lines are
//! already wrapped to the requested width so callers can add their own
//! prefixes and line caps.

use pulldown_cmark::{Event as Md, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::widgets::Palette;

/// Indentation of code block lines inside their block.
const CODE_INDENT: &str = "  ";
const QUOTE_BAR: &str = "│ ";
const BULLET: &str = "• ";
const COLUMN_GAP: &str = " │ ";

/// Wrapped, styled lines of `text` rendered as Markdown, at most `limit` of
/// them, plus whether more content existed past the limit. `compact` drops
/// the blank lines that separate blocks, for collapsed previews.
pub(super) fn render(
    text: &str,
    width: usize,
    compact: bool,
    limit: usize,
    palette: Palette,
) -> (Vec<Line<'static>>, bool) {
    let mut r = Renderer::new(width.max(1), compact, limit, palette);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        if r.cut {
            break;
        }
        r.event(event);
    }
    r.flush();
    (r.lines, r.cut)
}

/// One cell's styled content while a table is collected.
type Cell = Vec<Span<'static>>;

struct Table {
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    head_rows: usize,
}

struct ListLevel {
    /// Next number of an ordered list; `None` for bullets.
    next: Option<u64>,
    /// Width of the current item's marker, used as the hanging indent.
    hang: usize,
}

struct Renderer {
    width: usize,
    compact: bool,
    limit: usize,
    palette: Palette,
    lines: Vec<Line<'static>>,
    cut: bool,
    /// A blank line is owed before the next block's first line.
    gap: bool,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    /// The current item's marker, until its first line is emitted.
    marker: Option<String>,
    quote: usize,
    code: Option<String>,
    links: Vec<(String, usize)>,
    table: Option<Table>,
}

impl Renderer {
    fn new(width: usize, compact: bool, limit: usize, palette: Palette) -> Self {
        Self {
            width,
            compact,
            limit,
            palette,
            lines: Vec::new(),
            cut: false,
            gap: false,
            inline: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            marker: None,
            quote: 0,
            code: None,
            links: Vec::new(),
            table: None,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, style| acc.patch(*style))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if !text.is_empty() {
            self.inline.push(Span::styled(text.to_string(), style));
        }
    }

    fn event(&mut self, event: Md<'_>) {
        match event {
            Md::Start(tag) => self.start(tag),
            Md::End(tag) => self.end(tag),
            Md::Text(text) => {
                if let Some(code) = self.code.as_mut() {
                    code.push_str(&text);
                } else {
                    self.push_text(&text, self.style());
                }
            }
            Md::Code(text) => {
                let style = self.style().patch(self.palette.code());
                if self.palette.is_no_color() {
                    self.push_text(&format!("`{text}`"), style);
                } else {
                    self.push_text(&text, style);
                }
            }
            Md::Html(text) | Md::InlineHtml(text) => self.push_text(&text, self.style()),
            Md::SoftBreak | Md::HardBreak => self.push_text("\n", Style::default()),
            Md::Rule => {
                self.flush();
                let rule = "─".repeat(self.width.saturating_sub(self.prefix_width()).min(40));
                self.inline.push(Span::styled(rule, self.palette.quiet()));
                self.flush();
                self.gap = true;
            }
            Md::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.push_text(mark, self.palette.quiet());
            }
            // Footnotes, math and metadata are not enabled; keep any text.
            Md::FootnoteReference(text) | Md::InlineMath(text) | Md::DisplayMath(text) => {
                self.push_text(&text, self.style());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                let mut style = self.palette.heading();
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                // The owed gap sits outside the quote it precedes.
                self.open_block();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            Tag::HtmlBlock => self.flush(),
            Tag::List(first) => {
                self.flush();
                self.lists.push(ListLevel {
                    next: first,
                    hang: 0,
                });
            }
            Tag::Item => {
                self.flush();
                if let Some(level) = self.lists.last_mut() {
                    let marker = match level.next.as_mut() {
                        Some(n) => {
                            let marker = format!("{n}. ");
                            *n = n.saturating_add(1);
                            marker
                        }
                        None => BULLET.to_string(),
                    };
                    level.hang = marker.width();
                    self.marker = Some(marker);
                }
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Table {
                    rows: Vec::new(),
                    row: Vec::new(),
                    head_rows: 0,
                });
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.inline.len()));
                self.styles
                    .push(Style::new().add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => self.push_text("[image: ", self.palette.quiet()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                self.code_lines(&code);
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                // An empty item still shows its marker.
                if self.marker.is_some() {
                    self.emit_wrapped(Vec::new());
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_lines(table);
                }
                self.gap = true;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    if matches!(tag, TagEnd::TableHead) {
                        table.head_rows = table.rows.len();
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(table) = self.table.as_mut() {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, at)) = self.links.pop() {
                    let shown: String = self.inline[at.min(self.inline.len())..]
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect();
                    let bare = url.strip_prefix("mailto:").unwrap_or(&url);
                    if !url.is_empty() && shown != url && shown != bare {
                        self.push_text(&format!(" ({url})"), self.palette.quiet());
                    }
                }
            }
            TagEnd::Image => self.push_text("]", self.palette.quiet()),
            _ => {}
        }
    }

    fn prefix_width(&self) -> usize {
        self.quote * QUOTE_BAR.width() + self.lists.iter().map(|l| l.hang).sum::<usize>()
    }

    /// Prefix for the next emitted line; the first line of an item carries
    /// its marker, later lines hang under it.
    fn prefix(&mut self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for _ in 0..self.quote {
            spans.push(Span::styled(QUOTE_BAR, self.palette.quiet()));
        }
        let levels = self.lists.len();
        let outer: usize = self
            .lists
            .iter()
            .take(levels.saturating_sub(1))
            .map(|l| l.hang)
            .sum();
        if outer > 0 {
            spans.push(Span::raw(" ".repeat(outer)));
        }
        if let Some(level) = self.lists.last() {
            match self.marker.take() {
                Some(marker) => spans.push(Span::styled(marker, self.palette.quiet())),
                None if level.hang > 0 => spans.push(Span::raw(" ".repeat(level.hang))),
                None => {}
            }
        }
        spans
    }

    fn push_line(&mut self, line: Line<'static>) {
        if self.lines.len() >= self.limit {
            self.cut = true;
        } else {
            self.lines.push(line);
        }
    }

    /// Pay an owed blank line before a block's first line.
    fn open_block(&mut self) {
        if std::mem::take(&mut self.gap) && !self.compact && !self.lines.is_empty() {
            let bars = QUOTE_BAR.repeat(self.quote).trim_end().to_string();
            self.push_line(Line::styled(bars, self.palette.quiet()));
        }
    }

    fn flush(&mut self) {
        if self.table.is_some() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        if spans.iter().all(|span| span.content.trim().is_empty()) {
            return;
        }
        self.emit_wrapped(spans);
    }

    fn emit_wrapped(&mut self, spans: Vec<Span<'static>>) {
        self.open_block();
        let mut tokens = tokenize(&spans).into_iter().peekable();
        loop {
            let prefix = self.prefix();
            let avail = self
                .width
                .saturating_sub(prefix.iter().map(Span::width).sum())
                .max(1);
            let (line, more) = fill_line(prefix, &mut tokens, avail);
            self.push_line(line);
            if !more || self.cut {
                break;
            }
        }
    }

    fn code_lines(&mut self, code: &str) {
        self.open_block();
        let style = self.palette.code();
        for source in code.strip_suffix('\n').unwrap_or(code).split('\n') {
            let mut chars = source.chars().peekable();
            loop {
                let mut prefix = self.prefix();
                prefix.push(Span::raw(CODE_INDENT));
                let avail = self
                    .width
                    .saturating_sub(prefix.iter().map(Span::width).sum())
                    .max(1);
                let mut piece = String::new();
                let mut used = 0;
                while let Some(&ch) = chars.peek() {
                    let w = ch.width().unwrap_or(0);
                    if used + w > avail && used > 0 {
                        break;
                    }
                    piece.push(ch);
                    used += w;
                    chars.next();
                }
                prefix.push(Span::styled(piece, style));
                self.push_line(Line::from(prefix));
                if chars.peek().is_none() || self.cut {
                    break;
                }
            }
            if self.cut {
                return;
            }
        }
    }

    /// Aligned columns when the table fits, otherwise one wrapped line per row.
    fn table_lines(&mut self, table: Table) {
        self.open_block();
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.iter().map(Span::width).sum());
            }
        }
        let gap = COLUMN_GAP.width();
        let total = widths.iter().sum::<usize>() + gap * columns.saturating_sub(1);
        let fits = total + self.prefix_width() <= self.width;
        for (index, row) in table.rows.into_iter().enumerate() {
            let head = index < table.head_rows;
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, cell) in row.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(COLUMN_GAP, self.palette.quiet()));
                }
                let used: usize = cell.iter().map(Span::width).sum();
                for span in cell {
                    let span = if head {
                        span.patch_style(Modifier::BOLD)
                    } else {
                        span
                    };
                    spans.push(span);
                }
                if fits && i + 1 < columns {
                    spans.push(Span::raw(" ".repeat(widths[i].saturating_sub(used))));
                }
            }
            if fits {
                let mut line = self.prefix();
                line.extend(spans);
                self.push_line(Line::from(line));
            } else {
                // Cell contents may hold line breaks; keep the row on one flow.
                for span in &mut spans {
                    if span.content.contains('\n') {
                        span.content = span.content.replace('\n', " ").into();
                    }
                }
                self.emit_wrapped(spans);
            }
            if head && fits && index + 1 == table.head_rows {
                let rule = widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                let mut line = self.prefix();
                line.push(Span::styled(rule, self.palette.quiet()));
                self.push_line(Line::from(line));
            }
            if self.cut {
                return;
            }
        }
    }
}

enum Token {
    Word(Vec<Span<'static>>),
    Space,
    Break,
}

/// Split styled text into words, collapsed spaces and explicit line breaks.
/// A word may span several styles (`**bold**,` stays one unbreakable word).
fn tokenize(spans: &[Span<'static>]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word: Vec<Span<'static>> = Vec::new();
    let finish = |word: &mut Vec<Span<'static>>, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word)));
        }
    };
    for span in spans {
        for ch in span.content.chars() {
            if ch == '\n' {
                finish(&mut word, &mut tokens);
                tokens.push(Token::Break);
            } else if ch.is_whitespace() {
                finish(&mut word, &mut tokens);
                if !matches!(tokens.last(), Some(Token::Space)) {
                    tokens.push(Token::Space);
                }
            } else {
                match word.last_mut() {
                    Some(last) if last.style == span.style => {
                        last.content.to_mut().push(ch);
                    }
                    _ => word.push(Span::styled(ch.to_string(), span.style)),
                }
            }
        }
    }
    finish(&mut word, &mut tokens);
    tokens
}

/// Fill one line of `avail` columns after `prefix`; reports whether tokens
/// remain for another line. A word wider than the line is split.
fn fill_line(
    mut line: Vec<Span<'static>>,
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>,
    avail: usize,
) -> (Line<'static>, bool) {
    let mut used = 0usize;
    let mut space = false;
    while let Some(token) = tokens.peek_mut() {
        match token {
            Token::Space => {
                space = used > 0;
                tokens.next();
            }
            Token::Break => {
                tokens.next();
                return (Line::from(line), tokens.peek().is_some());
            }
            Token::Word(pieces) => {
                let word_w: usize = pieces.iter().map(Span::width).sum();
                let sep = usize::from(space && used > 0);
                if used > 0 && used + sep + word_w > avail {
                    return (Line::from(line), true);
                }
                if sep == 1 {
                    line.push(Span::raw(" "));
                    used += 1;
                }
                space = false;
                if word_w <= avail.saturating_sub(used) {
                    line.append(pieces);
                    used += word_w;
                    tokens.next();
                    continue;
                }
                // Too wide even for an empty line: take what fits and leave
                // the rest of the word for the next line.
                let mut rest: Vec<Span<'static>> = Vec::new();
                for piece in pieces.drain(..) {
                    if !rest.is_empty() {
                        rest.push(piece);
                        continue;
                    }
                    let mut head = String::new();
                    let mut tail = String::new();
                    for ch in piece.content.chars() {
                        let w = ch.width().unwrap_or(0);
                        if tail.is_empty() && (used + w <= avail || used == 0) {
                            head.push(ch);
                            used += w;
                        } else {
                            tail.push(ch);
                        }
                    }
                    if !head.is_empty() {
                        line.push(Span::styled(head, piece.style));
                    }
                    if !tail.is_empty() {
                        rest.push(Span::styled(tail, piece.style));
                    }
                }
                if rest.is_empty() {
                    tokens.next();
                } else {
                    *pieces = rest;
                    return (Line::from(line), true);
                }
            }
        }
    }
    (Line::from(line), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn md(text: &str, width: usize) -> Vec<String> {
        plain(&render(text, width, false, 100, Palette::new(false)).0)
    }

    fn style_of(lines: &[Line<'static>], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains(needle))
            .map(|span| span.style)
            .unwrap_or_default()
    }

    #[test]
    fn emphasis_markers_become_styles() {
        let (lines, cut) = render(
            "Some **bold**, *italic* and `code`.",
            80,
            false,
            10,
            Palette::new(false),
        );
        assert!(!cut);
        assert_eq!(plain(&lines), ["Some bold, italic and code."]);
        assert!(
            style_of(&lines, "bold")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            style_of(&lines, "italic")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(style_of(&lines, "code"), Palette::new(false).code());
    }

    #[test]
    fn no_color_keeps_inline_code_backticks() {
        let (lines, _) = render("run `cargo test`", 80, false, 10, Palette::new(true));
        assert_eq!(plain(&lines), ["run `cargo test`"]);
        assert!(lines[0].spans.iter().all(|s| s.style.fg.is_none()));
    }

    #[test]
    fn headings_lists_and_quotes_are_laid_out() {
        let text = "# Title\n\nIntro line\n\n- one\n- two\n  1. nested\n\n> quoted";
        assert_eq!(
            md(text, 80),
            [
                "Title",
                "",
                "Intro line",
                "",
                "• one",
                "• two",
                "  1. nested",
                "",
                "│ quoted",
            ]
        );
    }

    #[test]
    fn list_items_wrap_under_their_marker() {
        assert_eq!(
            md("- alpha beta gamma delta", 12),
            ["• alpha beta", "  gamma", "  delta"]
        );
    }

    #[test]
    fn soft_breaks_keep_the_recorded_line_layout() {
        assert_eq!(
            md("first line\nsecond line", 80),
            ["first line", "second line"]
        );
    }

    #[test]
    fn code_blocks_keep_whitespace_and_split_long_lines() {
        let text = "```rust\nfn main() {\n    let x = 1;\n}\n```";
        assert_eq!(md(text, 80), ["  fn main() {", "      let x = 1;", "  }"]);
        assert_eq!(md("```\nabcdefgh\n```", 6), ["  abcd", "  efgh"]);
    }

    #[test]
    fn tables_align_when_they_fit_and_flow_when_not() {
        let text = "| a | bb |\n|---|---|\n| ccc | d |";
        assert_eq!(md(text, 80), ["a   │ bb", "────┼───", "ccc │ d"]);
        let narrow = md(text, 6);
        assert!(narrow.iter().all(|line| line.width() <= 6), "{narrow:?}");
        assert!(narrow.concat().contains("ccc"));
    }

    #[test]
    fn links_show_their_target_once() {
        assert_eq!(
            md(
                "see [docs](https://example.test/a) or <https://example.test/b>",
                80
            ),
            ["see docs (https://example.test/a) or https://example.test/b"]
        );
    }

    #[test]
    fn raw_html_is_shown_literally() {
        assert_eq!(md("<b>hi</b> there", 80), ["<b>hi</b> there"]);
        assert_eq!(md("<div>\nblock\n</div>", 80), ["<div>", "block", "</div>"]);
    }

    #[test]
    fn limit_reports_hidden_content_and_compact_drops_gaps() {
        let text = "one\n\ntwo\n\nthree";
        let (lines, cut) = render(text, 80, true, 2, Palette::new(false));
        assert_eq!(plain(&lines), ["one", "two"]);
        assert!(cut);
        let (lines, cut) = render(text, 80, true, 3, Palette::new(false));
        assert_eq!(plain(&lines), ["one", "two", "three"]);
        assert!(!cut);
    }

    #[test]
    fn every_line_fits_the_width() {
        let text = "## A heading that is rather long\n\n- [x] done item with a long tail\n- [ ] \
                    open\n\n> quote that wraps across lines\n\nlongwordwithoutanyspacesatall";
        for width in 1..30 {
            for line in md(text, width) {
                assert!(line.width() <= width.max(4), "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn unterminated_markup_from_a_cut_preview_is_safe() {
        let lines = md("Intro\n\n```py\nprint(1)\n**open bold [link](", 40);
        assert_eq!(lines[0], "Intro");
        assert!(lines.iter().any(|l| l.contains("print(1)")));
    }
}
