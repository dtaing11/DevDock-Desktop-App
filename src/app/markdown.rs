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

/// The document's vertical rhythm. Every gap in a rendered document is one
/// of these three, so the spacing between a heading and its paragraph, two
/// paragraphs, and two list items are related rather than each being whatever
/// number the block that drew them happened to use.
const SPACE_SM: f32 = 4.0;
const SPACE_MD: f32 = 8.0;
const SPACE_LG: f32 = 18.0;

/// Body size, and the base of the heading scale.
const TEXT: f32 = 13.5;

/// One parsed block. Markdown is block-structured, so rendering happens in
/// two passes: split into blocks, then render inline spans within each.
enum Block {
    Heading { level: u8, text: String },
    Paragraph(String),
    /// `(language, lines)` — language may be empty.
    Code { lang: String, lines: Vec<String> },
    /// `(marker, text)` where marker is the rendered bullet or number.
    ListItem { marker: String, text: String, indent: usize },
    /// The lines of one blockquote. Consecutive `>` lines are one quote, not
    /// one per line: a quoted paragraph drawn as five separate bars with five
    /// gaps is not a blockquote, it is a list of stubs.
    Quote(Vec<String>),
    Rule,
}

/// Renders `md` into `ui`.
pub fn render(ui: &mut egui::Ui, md: &str) {
    // Blocks set their own spacing, so egui's between-widget gap would add a
    // second, unrelated rhythm on top of it.
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, block) in parse(md).into_iter().enumerate() {
        let first = i == 0;
        match block {
            Block::Heading { level, text } => {
                // Weight separates a heading from a paragraph, so the sizes
                // no longer have to shout to be distinguishable. Six levels
                // collapse onto four: past the third, a document is better
                // served by a heavier face at body size than by three more
                // sizes nobody can tell apart.
                let size = match level {
                    1 => 22.0,
                    2 => 17.5,
                    3 => 15.0,
                    _ => TEXT,
                };
                // Space belongs above a heading, not below it: a heading
                // groups with the text it introduces. The first block in a
                // document gets none, so it does not start with a gap.
                if !first {
                    ui.add_space(if level <= 2 { SPACE_LG } else { SPACE_MD + SPACE_SM });
                }
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::fg(), size, true);
                ui.label(job);
                // Only the document title gets a hairline. Giving every
                // H2 one turns a normal README into a stack of rules.
                if level == 1 {
                    ui.add_space(SPACE_SM);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 1.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 0.0, theme::border());
                }
                ui.add_space(SPACE_MD);
            }
            Block::Paragraph(text) => {
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::fg(), TEXT, false);
                ui.label(job);
                ui.add_space(SPACE_MD);
            }
            Block::Quote(lines) => {
                // A left rule plus dimmed text, rather than trying to draw a
                // real blockquote frame. The rule is laid out *after* the
                // text is measured, so it spans the whole quote — every line
                // of it — instead of stopping after the first.
                ui.horizontal_top(|ui| {
                    let bar =
                        ui.allocate_exact_size(egui::vec2(2.0, 0.0), egui::Sense::hover());
                    ui.add_space(SPACE_MD);
                    let inner = ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for line in &lines {
                            if line.is_empty() {
                                ui.add_space(SPACE_SM);
                                continue;
                            }
                            let mut job = LayoutJob::default();
                            inline(&mut job, line, theme::fg_dim(), TEXT, false);
                            // Explicit wrap: a horizontal layout does not
                            // wrap by default, so a long quote would run off
                            // the panel.
                            ui.add(egui::Label::new(job).wrap());
                        }
                    });
                    // The rule is sized from what the text actually took, so
                    // it runs the whole height of the quote rather than
                    // leaving a stub next to the first line.
                    let rule = egui::Rect::from_min_size(
                        bar.0.min,
                        egui::vec2(2.0, inner.response.rect.height().max(TEXT)),
                    );
                    ui.painter().rect_filled(rule, 1.0, theme::border());
                });
                ui.add_space(SPACE_MD);
            }
            Block::ListItem { marker, text, indent } => {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = SPACE_MD;
                    ui.add_space(SPACE_MD + indent as f32 * 16.0);
                    // The marker is dim, not accent-coloured: a list of ten
                    // bullets in the app's one accent colour reads as ten
                    // things demanding attention.
                    ui.label(
                        RichText::new(marker)
                            .color(theme::fg_dim())
                            .font(egui::FontId::monospace(TEXT - 1.5)),
                    );
                    let mut job = LayoutJob::default();
                    inline(&mut job, &text, theme::fg(), TEXT, false);
                    // Explicit wrap, for the same reason as a quote: the
                    // marker sits beside the text in a horizontal layout,
                    // where egui extends rather than wraps by default.
                    ui.add(egui::Label::new(job).wrap());
                });
                ui.add_space(SPACE_SM);
            }
            Block::Code { lang, lines } => {
                let detected = detect_lang(&lang);
                egui::Frame::new()
                    .fill(theme::panel2())
                    .stroke(egui::Stroke::new(1.0_f32, theme::border()))
                    .corner_radius(theme::RADIUS_SM as f32)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        // Every code block is the same width. Letting each
                        // shrink to its longest line gives a document a
                        // ragged column of differently sized boxes.
                        ui.set_min_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for line in &lines {
                            let mut job = LayoutJob::default();
                            for span in
                                syntax::highlight_line(detected, line, theme::fg())
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
                                        color: theme::fg(),
                                        ..Default::default()
                                    },
                                );
                            }
                            ui.label(job);
                        }
                    });
                ui.add_space(SPACE_MD);
            }
            Block::Rule => {
                // A rule is a separator, not a section: the space around it
                // reads as one gap rather than two.
                ui.add_space(SPACE_MD);
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 1.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 0.0, theme::border());
                ui.add_space(SPACE_MD);
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
    // Whether the previous line was part of a blockquote. A blank line ends
    // one, so `> a` / blank / `> b` is two quotes rather than one with a gap.
    let mut in_quote = false;

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
            in_quote = false;
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
            let text = text.trim().to_string();
            match blocks.last_mut() {
                Some(Block::Quote(lines)) if in_quote => lines.push(text),
                _ => blocks.push(Block::Quote(vec![text])),
            }
            in_quote = true;
            continue;
        }
        in_quote = false;

        if let Some((marker, text)) = list_item(lean) {
            flush!();
            blocks.push(Block::ListItem { marker, text, indent: indent / 2 });
            continue;
        }

        // A lazy continuation: an indented line under a list item belongs to
        // that item. Without this, every wrapped bullet in a README breaks
        // out to the left margin as its own paragraph.
        if indent >= 2 && paragraph.is_empty() {
            if let Some(Block::ListItem { text, .. }) = blocks.last_mut() {
                text.push(' ');
                text.push_str(lean);
                continue;
            }
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
        let heavy = bold || strong;
        let color = if code {
            theme::teal()
        } else if heavy {
            theme::strong_fg()
        } else {
            color
        };
        // Real faces, so emphasis is emphasis. There is no bold-italic
        // bundled, and italic carries a phrase better than weight does, so
        // `***both***` renders italic rather than shipping a fourth file for
        // a construct almost nothing uses.
        let font_id = if code {
            FontId::monospace(size - 1.0)
        } else if italics {
            theme::italic(size)
        } else if heavy {
            theme::semibold(size)
        } else {
            FontId::proportional(size)
        };
        job.append(
            s,
            0.0,
            TextFormat {
                font_id,
                color,
                // Inline code reads as a chip, the way it does everywhere
                // else Markdown is rendered.
                background: if code { theme::panel2() } else { Color32::TRANSPARENT },
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
            // An opener cannot be followed by a space. Without this rule the
            // `*` in "a lone * asterisk" opens emphasis and swallows the
            // sentence up to the next asterisk — which is exactly what a
            // reader did not write.
            let opens = bytes
                .get(i + marker_len)
                .is_some_and(|c| !c.is_whitespace());
            if let Some(end) = opens
                .then(|| find_run(&bytes, i + marker_len, c, marker_len))
                .flatten()
            {
                push(job, &buf, false, false, false);
                buf.clear();
                let inner: String = bytes[i + marker_len..end].iter().collect();
                if double {
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
                                color: theme::teal(),
                                // A hairline under the label rather than a
                                // full-weight rule: a paragraph of links
                                // should not read as a stack of underscores.
                                underline: egui::Stroke::new(
                                    1.0_f32,
                                    theme::teal().gamma_multiply(0.5),
                                ),
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

/// Finds the run of `len` copies of `marker` that closes an emphasis span
/// opened at `start`.
///
/// A closer cannot be preceded by whitespace, the mirror of the rule for
/// openers: in "an unclosed *bold, and a stray * here" the second asterisk
/// follows a space, so it closes nothing and both are text.
fn find_run(chars: &[char], start: usize, marker: char, len: usize) -> Option<usize> {
    let mut i = start;
    while i + len <= chars.len() {
        if chars[i..i + len].iter().all(|c| *c == marker) {
            let after_text = chars[i - 1].is_whitespace();
            // Reject an empty span (`**` immediately closing).
            if i > start && !after_text {
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

    /// The spans `inline` produced, as `(text, bold, italic)`.
    fn spans(text: &str) -> Vec<(String, bool, bool)> {
        let mut job = LayoutJob::default();
        inline(&mut job, text, theme::fg(), TEXT, false);
        job.sections
            .iter()
            .map(|s| {
                let name = match &s.format.font_id.family {
                    egui::FontFamily::Name(n) => n.to_string(),
                    other => format!("{other:?}"),
                };
                (
                    job.text[s.byte_range.clone()].to_string(),
                    name == "semibold",
                    name == "italic",
                )
            })
            .collect()
    }

    #[test]
    fn emphasis_uses_a_real_face_rather_than_a_colour() {
        let spans = spans("plain **bold** and *italic* here");
        let bold = spans.iter().find(|(t, ..)| t == "bold").expect("no bold span");
        assert!(bold.1, "bold is not set in the semibold face");
        let italic = spans.iter().find(|(t, ..)| t == "italic").expect("no italic span");
        assert!(italic.2, "italic is not set in the italic face");
        // And ordinary text is left alone.
        assert!(spans.iter().any(|(t, b, i)| t.contains("plain") && !b && !i));
    }

    #[test]
    fn a_marker_with_a_space_after_it_is_not_emphasis() {
        // The sentence a reader actually wrote. Treating the first asterisk
        // as an opener italicises everything up to the next one.
        let spans = spans("a lone * asterisk, then an unclosed *bold");
        assert!(
            spans.iter().all(|(_, bold, italic)| !bold && !italic),
            "something was emphasised: {spans:?}"
        );
    }

    #[test]
    fn a_marker_with_a_space_before_it_closes_nothing() {
        let spans = spans("*opened but the closer has a space before it *");
        assert!(spans.iter().all(|(_, _, italic)| !italic), "{spans:?}");
    }

    #[test]
    fn consecutive_quoted_lines_are_one_blockquote() {
        // Five bars with five gaps is not a blockquote, it is a list of stubs.
        let md = "> first line\n> second line\n> third line\n";
        assert_eq!(kinds(md), ["quote"]);
        match &parse(md)[0] {
            Block::Quote(lines) => assert_eq!(lines.len(), 3, "{lines:?}"),
            _ => panic!("expected one quote block, got {:?}", kinds(md)),
        }
    }

    #[test]
    fn a_blank_line_ends_a_blockquote() {
        // Two quoted passages, not one with a hole in it.
        assert_eq!(kinds("> one\n\n> two\n"), ["quote", "quote"]);
        // And a paragraph between them separates them too.
        assert_eq!(kinds("> one\nplain\n> two\n"), ["quote", "para", "quote"]);
    }

    #[test]
    fn a_wrapped_list_item_stays_one_item() {
        // How every README writes a long bullet.
        let md = "- A bullet whose text is long enough\n  that it wraps in the source\n- Second\n";
        assert_eq!(kinds(md), ["item", "item"]);
        let blocks = parse(md);
        let Block::ListItem { text, .. } = &blocks[0] else { panic!("expected an item") };
        assert_eq!(text, "A bullet whose text is long enough that it wraps in the source");
    }

    #[test]
    fn an_indented_line_after_a_paragraph_is_still_that_paragraph() {
        let md = "A paragraph\n  continued while indented\n";
        assert_eq!(kinds(md), ["para"]);
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
        inline(&mut job, "a * b `c", theme::fg(), 13.0, false);
        assert!(job.text.contains("a * b `c"), "got {:?}", job.text);
    }

    #[test]
    fn inline_code_and_emphasis_are_extracted() {
        let mut job = LayoutJob::default();
        inline(&mut job, "see `src/git.rs` and **fix** it", theme::fg(), 13.0, false);
        // Markers are consumed; the content survives.
        assert!(job.text.contains("src/git.rs"));
        assert!(job.text.contains("fix"));
        assert!(!job.text.contains("**"));
        assert!(!job.text.contains('`'));
    }

    #[test]
    fn links_render_their_label() {
        let mut job = LayoutJob::default();
        inline(&mut job, "see [the docs](https://x.test/a) now", theme::fg(), 13.0, false);
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

    /// The showcase fixture, which is the manual test for the renderer: if
    /// it is in the repository claiming to cover every construct, a test
    /// should be the thing that keeps that claim true.
    fn showcase() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/markdown-showcase.md");
        std::fs::read_to_string(path).expect("the showcase fixture must exist")
    }

    #[test]
    fn the_showcase_exercises_every_block_type() {
        let md = showcase();
        let kinds = kinds(&md);
        for expected in ["heading", "para", "code", "item", "quote", "rule"] {
            assert!(kinds.contains(&expected), "the showcase has no {expected} block");
        }
        // Enough of each to be a real exercise rather than a token example.
        let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
        assert!(count("heading") >= 12, "headings: {}", count("heading"));
        assert!(count("code") >= 14, "code blocks: {}", count("code"));
        assert!(count("item") >= 18, "list items: {}", count("item"));
        // Consecutive `>` lines are one quote, so this counts quoted
        // passages rather than quoted lines.
        assert!(count("quote") >= 3, "quotes: {}", count("quote"));
        assert!(count("rule") >= 8, "rules: {}", count("rule"));
    }

    #[test]
    fn the_showcase_covers_every_highlighted_language() {
        let md = showcase();
        let langs: Vec<String> = parse(&md)
            .iter()
            .filter_map(|b| match b {
                Block::Code { lang, .. } => Some(lang.clone()),
                _ => None,
            })
            .collect();
        for expected in [
            "rust", "python", "go", "c", "js", "ts", "java", "dart", "toml", "json",
            "yaml", "sh", "markdown",
        ] {
            assert!(langs.iter().any(|l| l == expected), "no {expected} fence: {langs:?}");
        }
        // A fence with no info string and one with an unknown language both
        // have to survive, since that is what real documents contain.
        assert!(langs.iter().any(|l| l.is_empty()), "no bare fence: {langs:?}");
        assert!(langs.iter().any(|l| l == "brainfuck"), "no unknown-language fence");
    }

    #[test]
    fn the_showcases_unterminated_fence_does_not_swallow_the_document() {
        let md = showcase();
        let blocks = parse(&md);
        // The file ends on an unterminated fence deliberately. Everything
        // before it must still have parsed, and the fence itself becomes the
        // last block rather than eating the rest of the file.
        assert!(blocks.len() > 100, "only {} blocks parsed", blocks.len());
        assert!(
            matches!(blocks.last(), Some(Block::Code { .. })),
            "the last block should be the unterminated fence"
        );
    }

    #[test]
    fn the_showcase_renders_without_panicking() {
        let md = showcase();
        theme::run_test_ctx(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &md);
            });
        });
    }

    /// Renders `md` into a Ui of exactly `width` and reports the size the
    /// content actually took.
    ///
    /// A real `Context` (not the test one) because that one loads no
    /// fonts, and text with no glyphs has no width to measure. Two passes,
    /// since the first lays out before the font atlas is warm.
    fn rendered_size(md: &str, width: f32) -> egui::Vec2 {
        let ctx = egui::Context::default();
        // Constrain the *window*, not the Ui: that is how a narrow panel
        // reaches the renderer in the real app, and it leaves no doubt about
        // whether the constraint was applied.
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(width, 2000.0),
            )),
            ..Default::default()
        };
        let mut size = egui::Vec2::ZERO;
        for _ in 0..2 {
            let _ = ctx.run(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    render(ui, md);
                    size = ui.min_rect().size();
                });
            });
        }
        size
    }

    /// Long prose has to wrap inside the panel, whatever block it is in.
    /// List items and quotes are laid out horizontally (marker beside text),
    /// and egui does not wrap text in a horizontal layout unless it is told
    /// to — so this is the test that keeps them from running off the edge.
    #[test]
    fn long_text_wraps_in_every_block_type() {
        const WIDTH: f32 = 300.0;
        let sentence = "This is a deliberately long line of prose that cannot possibly \
                        fit inside three hundred points of width and therefore has to \
                        wrap onto several lines to stay readable.";

        for (label, md) in [
            ("paragraph", sentence.to_string()),
            ("list item", format!("- {sentence}")),
            ("ordered item", format!("1. {sentence}")),
            ("quote", format!("> {sentence}")),
        ] {
            let size = rendered_size(&md, WIDTH);
            assert!(
                size.x <= WIDTH + 1.0,
                "{label} overflowed its panel: {}pt wide in a {WIDTH}pt ui",
                size.x
            );
            assert!(
                size.y > 40.0,
                "{label} did not wrap: {}pt tall, so it is still one line",
                size.y
            );
        }
    }

    /// A code block must not push the document wider than the panel. Code
    /// is not wrapped — that would corrupt how it reads — so a long line has
    /// to scroll inside its own block instead of overflowing the view.
    #[test]
    fn a_long_code_line_stays_inside_the_panel() {
        const WIDTH: f32 = 300.0;
        let md = format!("```rust\nlet x = \"{}\";\n```\n", "y".repeat(300));
        let size = rendered_size(&md, WIDTH);
        assert!(
            size.x <= WIDTH + 1.0,
            "the code block overflowed the panel: {}pt wide in a {WIDTH}pt ui",
            size.x
        );
    }
}
