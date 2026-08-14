//! Lightweight per-line syntax highlighting for diff views.
//!
//! This is not a full parser: each line is tokenized independently with
//! language-aware keywords, strings, comments, and numbers. That is the
//! right trade-off for diffs, where lines arrive without surrounding
//! context and rendering must stay fast for thousands of lines.

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};

/// Languages with dedicated keyword tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Java,
    Dart,
    Python,
    JavaScript,
    Html,
    Css,
    Go,
    /// Fallback: minimal highlighting (strings, numbers, comments).
    Plain,
}

impl Lang {
    /// Detects the language from a file path's extension.
    pub fn from_path(path: &str) -> Lang {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "java" => Lang::Java,
            "dart" => Lang::Dart,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::JavaScript,
            "html" | "htm" | "xml" | "vue" | "svelte" => Lang::Html,
            "css" | "scss" | "sass" | "less" => Lang::Css,
            "go" => Lang::Go,
            _ => Lang::Plain,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn",
                "else", "enum", "extern", "fn", "for", "if", "impl", "in", "let",
                "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
                "self", "Self", "static", "struct", "super", "trait", "type",
                "unsafe", "use", "where", "while",
            ],
            Lang::Java => &[
                "abstract", "assert", "boolean", "break", "byte", "case", "catch",
                "char", "class", "const", "continue", "default", "do", "double",
                "else", "enum", "extends", "final", "finally", "float", "for",
                "if", "implements", "import", "instanceof", "int", "interface",
                "long", "native", "new", "package", "private", "protected",
                "public", "record", "return", "short", "static", "strictfp",
                "super", "switch", "synchronized", "this", "throw", "throws",
                "transient", "try", "var", "void", "volatile", "while",
            ],
            Lang::Dart => &[
                "abstract", "as", "assert", "async", "await", "break", "case",
                "catch", "class", "const", "continue", "covariant", "default",
                "deferred", "do", "dynamic", "else", "enum", "export", "extends",
                "extension", "external", "factory", "final", "finally", "for",
                "get", "if", "implements", "import", "in", "is", "late",
                "library", "mixin", "new", "on", "operator", "part", "required",
                "rethrow", "return", "sealed", "set", "static", "super",
                "switch", "this", "throw", "try", "typedef", "var", "void",
                "while", "with", "yield",
            ],
            Lang::Python => &[
                "and", "as", "assert", "async", "await", "break", "class",
                "continue", "def", "del", "elif", "else", "except", "finally",
                "for", "from", "global", "if", "import", "in", "is", "lambda",
                "match", "nonlocal", "not", "or", "pass", "raise", "return",
                "try", "while", "with", "yield",
            ],
            Lang::JavaScript => &[
                "async", "await", "break", "case", "catch", "class", "const",
                "continue", "debugger", "default", "delete", "do", "else",
                "enum", "export", "extends", "finally", "for", "function",
                "if", "implements", "import", "in", "instanceof", "interface",
                "let", "new", "of", "private", "protected", "public", "return",
                "static", "super", "switch", "this", "throw", "try", "type",
                "typeof", "var", "void", "while", "with", "yield",
            ],
            Lang::Go => &[
                "break", "case", "chan", "const", "continue", "default",
                "defer", "else", "fallthrough", "for", "func", "go", "goto",
                "if", "import", "interface", "map", "package", "range",
                "return", "select", "struct", "switch", "type", "var",
            ],
            // HTML and CSS are handled structurally, not via keywords.
            Lang::Html | Lang::Css | Lang::Plain => &[],
        }
    }

    /// Common built-in types / literals worth a distinct color.
    fn types(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128",
                "isize", "str", "u8", "u16", "u32", "u64", "u128", "usize",
                "String", "Vec", "Option", "Some", "None", "Result", "Ok",
                "Err", "Box", "Rc", "Arc", "true", "false",
            ],
            Lang::Java => &["String", "Integer", "Boolean", "Object", "List", "Map", "Set", "true", "false", "null"],
            Lang::Dart => &["int", "double", "num", "bool", "String", "List", "Map", "Set", "Future", "Stream", "Widget", "true", "false", "null"],
            Lang::Python => &["True", "False", "None", "self", "cls", "int", "str", "float", "bool", "list", "dict", "set", "tuple"],
            Lang::JavaScript => &["true", "false", "null", "undefined", "NaN", "Infinity", "console", "Promise", "Array", "Object", "Number", "Boolean"],
            Lang::Go => &[
                "bool", "byte", "complex64", "complex128", "error", "float32",
                "float64", "int", "int8", "int16", "int32", "int64", "rune",
                "string", "uint", "uint8", "uint16", "uint32", "uint64",
                "uintptr", "true", "false", "nil", "iota", "make", "new",
                "len", "cap", "append", "copy", "panic", "recover",
            ],
            Lang::Html | Lang::Css | Lang::Plain => &[],
        }
    }

    /// Line-comment prefix, when the language has one.
    fn line_comment(self) -> Option<&'static str> {
        match self {
            Lang::Rust | Lang::Java | Lang::Dart | Lang::JavaScript | Lang::Go => Some("//"),
            Lang::Python => Some("#"),
            Lang::Css | Lang::Html | Lang::Plain => None,
        }
    }
}

/// Syntax palette tuned for the app's dark "dock at night" theme.
mod palette {
    use egui::Color32;
    pub const KEYWORD: Color32 = Color32::from_rgb(0xc7, 0x8f, 0xff); // violet
    pub const TYPE: Color32 = Color32::from_rgb(0x6f, 0xc2, 0xff); // sky blue
    pub const STRING: Color32 = Color32::from_rgb(0xa8, 0xd8, 0x8a); // soft green
    pub const NUMBER: Color32 = Color32::from_rgb(0xff, 0xb8, 0x6b); // amber
    pub const COMMENT: Color32 = Color32::from_rgb(0x6b, 0x75, 0x85); // slate
    pub const FUNCTION: Color32 = Color32::from_rgb(0xff, 0xd7, 0x8a); // gold
    pub const TAG: Color32 = Color32::from_rgb(0xff, 0x8f, 0x8f); // coral (html tags / css selectors)
    pub const ATTR: Color32 = Color32::from_rgb(0x9a, 0xe6, 0xd2); // mint (attributes / css props)
    pub const PUNCT: Color32 = Color32::from_rgb(0x8a, 0x93, 0xa6); // dim
}

/// A colored fragment of one line.
#[derive(Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub color: Color32,
}

/// Tokenizes one line of code into colored spans.
///
/// `default_color` is used for identifiers and anything unrecognized so
/// diff coloring (add/del tint) still shows through on plain text.
pub fn highlight_line(lang: Lang, line: &str, default_color: Color32) -> Vec<Span> {
    match lang {
        Lang::Html => highlight_html(line, default_color),
        Lang::Css => highlight_css(line, default_color),
        _ => highlight_code(lang, line, default_color),
    }
}

fn push(spans: &mut Vec<Span>, text: &str, color: Color32) {
    if text.is_empty() {
        return;
    }
    // Merge with the previous span when the color matches to keep the
    // LayoutJob small.
    if let Some(last) = spans.last_mut() {
        if last.color == color {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(Span { text: text.to_string(), color });
}

fn highlight_code(lang: Lang, line: &str, default_color: Color32) -> Vec<Span> {
    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    // Whole-line comment detection (also mid-line).
    let comment = lang.line_comment();

    while i < chars.len() {
        let c = chars[i];

        // Comment to end of line. For Python's '#', only when not inside
        // a string (strings are consumed below before we get here).
        if let Some(prefix) = comment {
            let matches_prefix = chars[i..]
                .iter()
                .zip(prefix.chars())
                .filter(|(a, b)| **a == *b)
                .count()
                == prefix.len();
            if matches_prefix {
                let rest: String = chars[i..].iter().collect();
                push(&mut spans, &rest, palette::COMMENT);
                break;
            }
        }

        // Strings: " ' and ` (JS template literals).
        if c == '"' || c == '\'' || (c == '`' && lang == Lang::JavaScript) {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let text: String = chars[start..i.min(chars.len())].iter().collect();
            push(&mut spans, &text, palette::STRING);
            continue;
        }

        // Numbers (integer, float, hex).
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            push(&mut spans, &text, palette::NUMBER);
            continue;
        }

        // Identifiers / keywords.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let color = if lang.keywords().contains(&word.as_str()) {
                palette::KEYWORD
            } else if lang.types().contains(&word.as_str()) {
                palette::TYPE
            } else if chars.get(i) == Some(&'(') {
                palette::FUNCTION
            } else {
                default_color
            };
            push(&mut spans, &word, color);
            continue;
        }

        // Punctuation and everything else.
        let color = if "{}()[]<>;,.:=+-*/&|!?%^~@".contains(c) {
            palette::PUNCT
        } else {
            default_color
        };
        push(&mut spans, &c.to_string(), color);
        i += 1;
    }
    spans
}

/// HTML: tags coral, attributes mint, attribute values green.
fn highlight_html(line: &str, default_color: Color32) -> Vec<Span> {
    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_tag = false;

    while i < chars.len() {
        let c = chars[i];
        if !in_tag {
            if c == '<' {
                in_tag = true;
                // consume "<" or "</" plus the tag name
                let start = i;
                i += 1;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                }
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                push(&mut spans, &text, palette::TAG);
            } else {
                push(&mut spans, &c.to_string(), default_color);
                i += 1;
            }
            continue;
        }
        // Inside a tag.
        if c == '>' {
            in_tag = false;
            push(&mut spans, ">", palette::TAG);
            i += 1;
        } else if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            i = (i + 1).min(chars.len());
            let text: String = chars[start..i].iter().collect();
            push(&mut spans, &text, palette::STRING);
        } else if c.is_alphabetic() || c == '-' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            push(&mut spans, &text, palette::ATTR);
        } else {
            push(&mut spans, &c.to_string(), palette::PUNCT);
            i += 1;
        }
    }
    spans
}

/// CSS: selectors coral, properties mint, values default, strings green.
fn highlight_css(line: &str, default_color: Color32) -> Vec<Span> {
    let mut spans = Vec::new();
    // Property lines look like "  name: value;".
    if let Some((prop, value)) = line.split_once(':') {
        let name = prop.trim_start();
        if !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            let indent_len = prop.len() - name.len();
            push(&mut spans, &prop[..indent_len], default_color);
            push(&mut spans, name, palette::ATTR);
            push(&mut spans, ":", palette::PUNCT);
            // Color numbers inside the value; keep the rest default.
            for span in highlight_code(Lang::Plain, value, default_color) {
                push(&mut spans, &span.text, span.color);
            }
            return spans;
        }
    }
    // Selector / at-rule lines.
    let trimmed = line.trim_start();
    if trimmed.starts_with('@') || trimmed.ends_with('{') || trimmed.starts_with('.') || trimmed.starts_with('#') {
        push(&mut spans, line, palette::TAG);
        return spans;
    }
    push(&mut spans, line, default_color);
    spans
}

/// Builds an egui [`LayoutJob`] for one diff line: the +/- marker keeps
/// the diff color, the rest of the line gets syntax colors.
pub fn diff_line_job(
    lang: Lang,
    line: &str,
    diff_color: Color32,
    font: FontId,
    syntax: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = |color: Color32| TextFormat { font_id: font.clone(), color, ..Default::default() };

    // Structural lines keep pure diff coloring.
    let structural = line.starts_with("@@")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ");
    if structural || !syntax {
        job.append(line, 0.0, format(diff_color));
        return job;
    }

    let (marker, code) = match line.chars().next() {
        Some(c @ ('+' | '-')) => (Some(c), &line[1..]),
        _ => (None, line),
    };
    if let Some(marker) = marker {
        job.append(&marker.to_string(), 0.0, format(diff_color));
    }
    for span in highlight_line(lang, code, diff_color) {
        job.append(&span.text, 0.0, format(span.color));
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: Color32 = Color32::from_rgb(0xe8, 0xe3, 0xd8);

    fn colors_of(spans: &[Span], text: &str) -> Option<Color32> {
        spans.iter().find(|s| s.text.contains(text)).map(|s| s.color)
    }

    #[test]
    fn detects_languages_from_paths() {
        assert_eq!(Lang::from_path("src/main.rs"), Lang::Rust);
        assert_eq!(Lang::from_path("App.java"), Lang::Java);
        assert_eq!(Lang::from_path("lib/app.dart"), Lang::Dart);
        assert_eq!(Lang::from_path("tool.py"), Lang::Python);
        assert_eq!(Lang::from_path("index.tsx"), Lang::JavaScript);
        assert_eq!(Lang::from_path("page.html"), Lang::Html);
        assert_eq!(Lang::from_path("style.scss"), Lang::Css);
        assert_eq!(Lang::from_path("main.go"), Lang::Go);
        assert_eq!(Lang::from_path("README.md"), Lang::Plain);
    }

    #[test]
    fn rust_keywords_types_strings_and_comments() {
        let spans = highlight_line(Lang::Rust, "pub fn go(x: u32) { // note", FG);
        assert_eq!(colors_of(&spans, "pub"), Some(palette::KEYWORD));
        assert_eq!(colors_of(&spans, "fn"), Some(palette::KEYWORD));
        assert_eq!(colors_of(&spans, "u32"), Some(palette::TYPE));
        assert_eq!(colors_of(&spans, "// note"), Some(palette::COMMENT));

        let spans = highlight_line(Lang::Rust, r#"let s = "hi"; let n = 42;"#, FG);
        assert_eq!(colors_of(&spans, "\"hi\""), Some(palette::STRING));
        assert_eq!(colors_of(&spans, "42"), Some(palette::NUMBER));
    }

    #[test]
    fn python_uses_hash_comments() {
        let spans = highlight_line(Lang::Python, "def f():  # docs", FG);
        assert_eq!(colors_of(&spans, "def"), Some(palette::KEYWORD));
        assert_eq!(colors_of(&spans, "# docs"), Some(palette::COMMENT));
    }

    #[test]
    fn html_tags_and_attributes() {
        let spans = highlight_line(Lang::Html, r#"<div class="box">"#, FG);
        assert_eq!(colors_of(&spans, "<div"), Some(palette::TAG));
        assert_eq!(colors_of(&spans, "class"), Some(palette::ATTR));
        assert_eq!(colors_of(&spans, "\"box\""), Some(palette::STRING));
    }

    #[test]
    fn css_properties_and_selectors() {
        let spans = highlight_line(Lang::Css, "  color: #fff;", FG);
        assert_eq!(colors_of(&spans, "color"), Some(palette::ATTR));
        let spans = highlight_line(Lang::Css, ".card {", FG);
        assert_eq!(spans[0].color, palette::TAG);
    }

    #[test]
    fn function_calls_get_gold() {
        let spans = highlight_line(Lang::Go, "fmt.Println(x)", FG);
        assert_eq!(colors_of(&spans, "Println"), Some(palette::FUNCTION));
    }

    #[test]
    fn diff_marker_keeps_diff_color_and_code_is_highlighted() {
        let job = diff_line_job(
            Lang::Rust,
            "+    let x = 1;",
            Color32::GREEN,
            FontId::monospace(12.0),
            true,
        );
        // First section is the "+" marker in diff green.
        assert_eq!(job.sections[0].format.color, Color32::GREEN);
        assert_eq!(&job.text[..1], "+");
        // Structural lines stay pure diff-colored.
        let job = diff_line_job(
            Lang::Rust,
            "@@ -1,3 +1,4 @@",
            Color32::GRAY,
            FontId::monospace(12.0),
            true,
        );
        assert_eq!(job.sections.len(), 1);
    }
}
