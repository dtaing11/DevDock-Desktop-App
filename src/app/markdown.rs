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
                // A real scale. With no bold face available, size is the
                // only thing that separates a heading from a paragraph, so
                // the steps have to be big enough to read as steps.
                let size = match level {
                    1 => 25.0,
                    2 => 20.0,
                    3 => 16.5,
                    4 => 15.0,
                    _ => 13.5,
                };
                // Space belongs above a heading, not below it: a heading
                // groups with the text it introduces.
                ui.add_space(match level {
                    1 => 20.0,
                    2 => 17.0,
                    3 => 13.0,
                    _ => 10.0,
                });
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::FG, size, true);
                ui.label(job);
                // Only the document title gets a hairline. Giving every
                // H2 one turns a normal README into a stack of rules.
                if level == 1 {
                    ui.add_space(3.0);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 1.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 0.0, theme::BORDER);
                }
                ui.add_space(5.0);
            }
            Block::Paragraph(text) => {
                let mut job = LayoutJob::default();
                inline(&mut job, &text, theme::FG, 13.5, false);
                ui.label(job);
                ui.add_space(9.0);
            }
            Block::Quote(text) => {
                // A left rule plus dimmed text, rather than trying to draw a
                // real blockquote frame. The rule is laid out *after* the
                // text is measured, so it spans a wrapped quote instead of
                // stopping after one line.
                ui.horizontal_top(|ui| {
                    let bar = ui.allocate_exact_size(
                        egui::vec2(3.0, 0.0),
                        egui::Sense::hover(),
                    );
                    ui.add_space(8.0);
                    let mut job = LayoutJob::default();
                    inline(&mut job, &text, theme::FG_DIM, 13.5, false);
                    // Explicit wrap: a horizontal layout does not wrap text
                    // by default, so a long quote would run off the panel.
                    let response = ui.add(egui::Label::new(job).wrap());
                    let rule = egui::Rect::from_min_size(
                        bar.0.min,
                        egui::vec2(3.0, response.rect.height()),
                    );
                    ui.painter().rect_filled(rule, 1.0, theme::EMBER_DEEP);
                });
                ui.add_space(9.0);
            }
            Block::ListItem { marker, text, indent } => {
                ui.horizontal_top(|ui| {
                    ui.add_space(10.0 + indent as f32 * 14.0);
                    ui.label(RichText::new(marker).color(theme::EMBER).monospace().size(13.0));
                    let mut job = LayoutJob::default();
                    inline(&mut job, &text, theme::FG, 13.5, false);
                    // Explicit wrap, for the same reason as a quote: the
                    // marker sits beside the text in a horizontal layout,
                    // where egui extends rather than wraps by default.
                    ui.add(egui::Label::new(job).wrap());
                });
                ui.add_space(4.0);
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
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
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
                    FontId::monospace(size - 1.0)
                } else {
                    FontId::proportional(size)
                },
                color,
                italics,
                // No bold face is loaded, so weight is faked with colour and
                // a little tracking. It is not a real bold, but it is the
                // difference between "I can see the emphasis" and not.
                extra_letter_spacing: if bold || strong { 0.4 } else { 0.0 },
                // Inline code reads as a chip, the way it does everywhere
                // else Markdown is rendered.
                background: if code { theme::PANEL2 } else { Color32::TRANSPARENT },
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
        assert!(count("quote") >= 4, "quotes: {}", count("quote"));
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
        egui::__run_test_ctx(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &md);
            });
        });
    }

    /// Renders `md` into a Ui of exactly `width` and reports the size the
    /// content actually took.
    ///
    /// A real `Context` (not `__run_test_ctx`) because that one loads no
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
