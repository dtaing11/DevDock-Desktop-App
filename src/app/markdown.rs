//! A small Markdown renderer for AI review output.
//!
//! Reviews written in a project's own house style arrive as Markdown, so the
//! app has to render it rather than dump the source. This covers the subset a
//! code review actually uses — headings, emphasis, inline code, fenced code
//! blocks, lists, blockquotes, rules, and links — and falls back to plain
//! text for anything else, which is the right failure mode for content that
//! is only ever *displayed*.
//!
//! Fenced code blocks reuse [`super::syntax`], so a review quoting Rust or
//! Dart gets the same highlighting as the diff views.

use super::{syntax, theme};
use egui::{text::LayoutJob, Color32, FontId, RichText, TextFormat};

/// Emphasis colour. The app loads no bold face, so `**bold**` and headings
/// read as a brighter tone than [`theme::FG`] instead of a heavier weight.
const STRONG: Color32 = Color32::from_rgb(0xff, 0xfb, 0xf2);

/// One parsed block. Markdown is block-structured, so rendering happens in
/// two passes: split into blocks, then render inline spans within each.
enum Block {
    Heading { level: u8, text: String },
    Paragraph(String),
    /// `(language, lines)` — language may be empty.
    Code { lang: String, lines: Vec<String> },
    /// `(marker, text)` where marker is the rendered bullet or number.
    ListItem { marker: String, text: String, indent: usize },
    Quote(String),
    Rule,
}

/// Renders `md` into `ui`.
pub fn render(ui: &mut egui::Ui, md: &str) {
    for block in parse(md) {
        match block {
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 19.0,
                    2 => 16.5,
                    _ => 14.5,
                };
                ui.add_space(if level <= 2 { 8.0 } else { 6.0 });
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::FG, size, true);
                ui.label(job);
                ui.add_space(2.0);
            }
            Block::Paragraph(text) => {
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::FG, 13.0, false);
                ui.label(job);
                ui.add_space(5.0);
            }
            Block::Quote(text) => {
                // A left rule plus dimmed text, rather than trying to draw a
                // real blockquote frame.
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(3.0, 18.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 1.0, theme::EMBER_DEEP);
                    let mut job = LayoutJob::default();
                    inline(&mut job, &text, theme::FG_DIM, 13.0, false);
                    ui.label(job);
                });
                ui.add_space(5.0);
            }
            Block::ListItem { marker, text, indent } => {
                ui.horizontal_top(|ui| {
                    ui.add_space(10.0 + indent as f32 * 14.0);
                    ui.label(RichText::new(marker).color(theme::EMBER).monospace().size(13.0));
                    let mut job = LayoutJob::default();
                    inline(&mut job, &text, theme::FG, 13.0, false);
                    ui.label(job);
                });
                ui.add_space(2.0);
            }
            Block::Code { lang, lines } => {
                let detected = detect_lang(&lang);
                egui::Frame::new()
                    .fill(theme::PANEL2)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(theme::RADIUS_SM as f32)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        for line in &lines {
                            let mut job = LayoutJob::default();
                            for span in
                                syntax::highlight_line(detected, line, theme::FG)
                            {
                                job.append(
                                    &span.text,
                                    0.0,
                                    TextFormat {
                                        font_id: FontId::monospace(12.5),
                                        color: span.color,
                                        ..Default::default()
                                    },
                                );
                            }
                            // An empty line still needs height.
                            if line.is_empty() {
                                job.append(
                                    " ",
                                    0.0,
                                    TextFormat {
                                        font_id: FontId::monospace(12.5),
                                        color: theme::FG,
                                        ..Default::default()
                                    },
                                );
                            }
                            ui.label(job);
                        }
                    });
                ui.add_space(6.0);
            }
            Block::Rule => {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
            }
        }
    }
}

/// Maps a fence's info string onto a highlighter language. `from_path` keys
/// off extensions, so a bare name is turned into one.
fn detect_lang(info: &str) -> syntax::Lang {
    let name = info.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let ext = match name.as_str() {
        "rust" | "rs" => "rs",
        "dart" => "dart",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yml",
        "python" | "py" => "py",
        "javascript" | "js" => "js",
        "typescript" | "ts" => "ts",
        "java" => "java",
        "go" => "go",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "sh" | "bash" | "shell" | "zsh" => "sh",
        "md" | "markdown" => "md",
        other => other,
    };
    syntax::Lang::from_path(&format!("x.{ext}"))
}

fn parse(md: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut lines = md.lines().peekable();

    // Paragraph lines accumulate until a blank line or a block-level marker.
    macro_rules! flush {
        () => {
            if !paragraph.is_empty() {
                blocks.push(Block::Paragraph(paragraph.join(" ")));
                paragraph.clear();
            }
        };
    }

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        let lean = trimmed.trim_start();
        let indent = trimmed.len().saturating_sub(lean.len());

        if lean.is_empty() {
            flush!();
            continue;
        }

        // Fenced code block: consume until the closing fence (or the end, so
        // an unterminated fence still renders rather than swallowing the rest).
        if let Some(info) = lean.strip_prefix("```") {
            flush!();
            let mut code = Vec::new();
            for next in lines.by_ref() {
                if next.trim_start().starts_with("```") {
                    break;
                }
                code.push(next.to_string());
            }
            blocks.push(Block::Code { lang: info.trim().to_string(), lines: code });
            continue;
        }

        // Thematic break, before list parsing so `---` is not read as a bullet.
        if is_rule(lean) {
            flush!();
            blocks.push(Block::Rule);
            continue;
        }

        if let Some(rest) = heading(lean) {
            flush!();
            blocks.push(Block::Heading { level: rest.0, text: rest.1 });
            continue;
        }

        if let Some(text) = lean.strip_prefix("> ").or_else(|| lean.strip_prefix(">")) {
            flush!();
            blocks.push(Block::Quote(text.trim().to_string()));
            continue;
        }

        if let Some((marker, text)) = list_item(lean) {
            flush!();
            blocks.push(Block::ListItem { marker, text, indent: indent / 2 });
            continue;
        }

        paragraph.push(lean.to_string());
    }
    flush!();
    blocks
}

fn heading(line: &str) -> Option<(u8, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, rest.trim().to_string()))
}

/// `---`, `***`, `___` (three or more, nothing else on the line).
fn is_rule(line: &str) -> bool {
    let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

fn list_item(line: &str) -> Option<(String, String)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return Some(("•".to_string(), rest.trim().to_string()));
        }
    }
    // Ordered: `1. text` / `12) text`
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &line[digits..];
        for sep in [". ", ") "] {
            if let Some(text) = rest.strip_prefix(sep) {
                return Some((format!("{}.", &line[..digits]), text.trim().to_string()));
            }
        }
    }
    None
}

/// Appends inline spans (`**bold**`, `*italic*`, `` `code` ``, `[text](url)`)
/// to `job`. Unmatched markers are emitted literally rather than eating the
/// rest of the line.
fn inline(job: &mut LayoutJob, text: &str, color: Color32, size: f32, strong: bool) {
    let push = |job: &mut LayoutJob, s: &str, bold: bool, italics: bool, code: bool| {
        if s.is_empty() {
            return;
        }
        // No bold face is loaded, so emphasis reads as a brighter tone rather
        // than a heavier weight. Changing the font here would be a no-op.
        let color = if code {
            theme::TEAL
        } else if bold || strong {
            STRONG
        } else {
            color
        };
        job.append(
            s,
            0.0,
            TextFormat {
                font_id: if code {
                    FontId::monospace(size - 0.5)
                } else {
                    FontId::proportional(size)
                },
                color,
                italics,
                ..Default::default()
            },
        );
    };

    let bytes: Vec<char> = text.chars().collect();
    let mut buf = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Inline code wins over emphasis, matching Markdown precedence.
        if c == '`' {
            if let Some(end) = find_from(&bytes, i + 1, '`') {
                push(job, &buf, false, false, false);
                buf.clear();
                let code: String = bytes[i + 1..end].iter().collect();
                push(job, &code, false, false, true);
                i = end + 1;
                continue;
            }
        }
        if c == '*' || c == '_' {
            let double = i + 1 < bytes.len() && bytes[i + 1] == c;
            let marker_len = if double { 2 } else { 1 };
            if let Some(end) = find_run(&bytes, i + marker_len, c, marker_len) {
                push(job, &buf, false, false, false);
                buf.clear();
                let inner: String = bytes[i + marker_len..end].iter().collect();
                if double {
                    // Bold: brighten instead of changing weight.
                    push(job, &inner, true, false, false);
                } else {
                    push(job, &inner, false, true, false);
                }
                i = end + marker_len;
                continue;
            }
        }
        if c == '[' {
            if let (Some(close), ) = (find_from(&bytes, i + 1, ']'), ) {
                if close + 1 < bytes.len() && bytes[close + 1] == '(' {
                    if let Some(paren) = find_from(&bytes, close + 2, ')') {
                        push(job, &buf, false, false, false);
                        buf.clear();
                        let label: String = bytes[i + 1..close].iter().collect();
                        job.append(
                            &label,
                            0.0,
                            TextFormat {
                                font_id: FontId::proportional(size),
                                color: theme::TEAL,
                                underline: egui::Stroke::new(1.0_f32, theme::TEAL),
                                ..Default::default()
                            },
                        );
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    push(job, &buf, false, false, false);
}

fn find_from(chars: &[char], start: usize, needle: char) -> Option<usize> {
    (start..chars.len()).find(|&i| chars[i] == needle)
}

/// Finds a run of `len` copies of `marker` at or after `start`.
fn find_run(chars: &[char], start: usize, marker: char, len: usize) -> Option<usize> {
    let mut i = start;
    while i + len <= chars.len() {
        if chars[i..i + len].iter().all(|c| *c == marker) {
            // Reject an empty span (`**` immediately closing).
            if i > start {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(md: &str) -> Vec<&'static str> {
        parse(md)
            .iter()
            .map(|b| match b {
                Block::Heading { .. } => "heading",
                Block::Paragraph(_) => "para",
                Block::Code { .. } => "code",
                Block::ListItem { .. } => "item",
                Block::Quote(_) => "quote",
                Block::Rule => "rule",
            })
            .collect()
    }

    #[test]
    fn parses_the_block_types_a_review_uses() {
        let md = "## Verdict\n\nLooks wrong.\n\n- one\n- two\n\n> careful\n\n---\n";
        assert_eq!(kinds(md), ["heading", "para", "item", "item", "quote", "rule"]);
    }

    #[test]
    fn fenced_code_is_one_block_and_keeps_its_lines() {
        let blocks = parse("text\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\nafter\n");
        let code = blocks
            .iter()
            .find_map(|b| match b {
                Block::Code { lang, lines } => Some((lang.clone(), lines.clone())),
                _ => None,
            })
            .expect("expected a code block");
        assert_eq!(code.0, "rust");
        assert_eq!(code.1, vec!["let a = 1;", "", "let b = 2;"]);
        assert_eq!(kinds("text\n\n```rust\nx\n```\nafter\n"), ["para", "code", "para"]);
    }

    /// An unterminated fence must not swallow the rest of the review.
    #[test]
    fn unterminated_fence_still_renders() {
        let blocks = parse("intro\n\n```\ndangling\n");
        assert_eq!(kinds("intro\n\n```\ndangling\n"), ["para", "code"]);
        assert!(matches!(&blocks[1], Block::Code { lines, .. } if lines == &["dangling"]));
    }

    #[test]
    fn ordered_and_bulleted_items_both_parse() {
        assert_eq!(kinds("- a\n1. b\n2) c\n"), ["item", "item", "item"]);
        assert_eq!(list_item("1. hi").unwrap().0, "1.");
        assert_eq!(list_item("- hi").unwrap().0, "•");
        // A rule is not a bullet.
        assert!(list_item("---").is_none());
        assert_eq!(kinds("---\n"), ["rule"]);
    }

    #[test]
    fn paragraph_lines_join_until_a_blank_line() {
        assert_eq!(kinds("one\ntwo\n\nthree\n"), ["para", "para"]);
        let blocks = parse("one\ntwo\n");
        assert!(matches!(&blocks[0], Block::Paragraph(p) if p == "one two"));
    }

    #[test]
    fn headings_need_a_space_and_stop_at_six() {
        assert_eq!(heading("# a").unwrap().0, 1);
        assert_eq!(heading("###### a").unwrap().0, 6);
        assert!(heading("####### a").is_none());
        // `#tag` is prose, not a heading.
        assert!(heading("#nothing").is_none());
    }

    /// Unmatched inline markers must be emitted literally, not eat the line.
    #[test]
    fn unmatched_inline_markers_are_literal() {
        let mut job = LayoutJob::default();
        inline(&mut job, "a * b `c", theme::FG, 13.0, false);
        assert!(job.text.contains("a * b `c"), "got {:?}", job.text);
    }

    #[test]
    fn inline_code_and_emphasis_are_extracted() {
        let mut job = LayoutJob::default();
        inline(&mut job, "see `src/git.rs` and **fix** it", theme::FG, 13.0, false);
        // Markers are consumed; the content survives.
        assert!(job.text.contains("src/git.rs"));
        assert!(job.text.contains("fix"));
        assert!(!job.text.contains("**"));
        assert!(!job.text.contains('`'));
    }

    #[test]
    fn links_render_their_label() {
        let mut job = LayoutJob::default();
        inline(&mut job, "see [the docs](https://x.test/a) now", theme::FG, 13.0, false);
        assert!(job.text.contains("the docs"));
        assert!(!job.text.contains("https://"), "url should not be shown: {:?}", job.text);
    }

    #[test]
    fn fence_info_maps_onto_a_highlighter_language() {
        assert_eq!(detect_lang("rust"), syntax::Lang::from_path("x.rs"));
        assert_eq!(detect_lang("bash"), syntax::Lang::from_path("x.sh"));
        // Unknown languages fall back rather than panicking.
        let _ = detect_lang("brainfuck");
        let _ = detect_lang("");
    }
}
