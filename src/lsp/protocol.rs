//! Language Server Protocol wire format: framing, URIs, positions, and the
//! subset of the protocol's types this client actually uses.
//!
//! Two things here are easy to get wrong and expensive to debug later, so
//! both are implemented deliberately and tested:
//!
//! - **Framing.** Messages are `Content-Length: N\r\n\r\n` followed by
//!   exactly N *bytes* of JSON. Counting characters instead of bytes works
//!   until the first non-ASCII identifier arrives.
//! - **Positions.** LSP counts a character offset in **UTF-16 code units**,
//!   not bytes and not chars. An emoji in a comment shifts every column on
//!   its line, which shows up as diagnostics underlining the wrong text.
//!   [`utf16_to_byte`] and [`byte_to_utf16`] are the only sanctioned way to
//!   cross that boundary.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Reads one framed message. `Ok(None)` means the stream ended cleanly,
/// which is what a server exiting looks like.
pub fn read_message(reader: &mut impl BufRead) -> Result<Option<serde_json::Value>, String> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|e| format!("LSP read failed: {e}"))?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let length = length.ok_or("LSP message had no Content-Length header")?;
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).map_err(|e| format!("LSP read failed: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| format!("LSP sent invalid JSON: {e}"))
}

/// Writes one framed message.
pub fn write_message(writer: &mut impl Write, message: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())
        .map_err(|e| format!("LSP write failed: {e}"))?;
    writer.write_all(&body).map_err(|e| format!("LSP write failed: {e}"))?;
    writer.flush().map_err(|e| format!("LSP write failed: {e}"))
}

// ---------------------------------------------------------------------------
// URIs
// ---------------------------------------------------------------------------

/// `file://` URI for an absolute path, percent-encoding what must be encoded.
pub fn path_to_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    let text = path.to_string_lossy();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            // Windows drive colons stay readable; everything else is escaped.
            b':' => out.push(':'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Path from a `file://` URI, undoing percent-encoding.
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut chars = rest.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hex: Vec<u8> = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(text) = std::str::from_utf8(&hex) {
                    if let Ok(value) = u8::from_str_radix(text, 16) {
                        bytes.push(value);
                        continue;
                    }
                }
            }
            return None;
        }
        bytes.push(b);
    }
    String::from_utf8(bytes).ok().map(std::path::PathBuf::from)
}

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

/// A zero-based line and UTF-16 character offset, as LSP counts them.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A half-open span between two [`Position`]s.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// UTF-16 offset within `line` for a byte offset, for sending a cursor to
/// the server.
pub fn byte_to_utf16(line: &str, byte_offset: usize) -> u32 {
    let end = byte_offset.min(line.len());
    line[..end].encode_utf16().count() as u32
}

/// Byte offset within `line` for a UTF-16 offset, for placing what a server
/// sent back. Offsets past the end clamp to the end rather than panicking:
/// servers legitimately point one past the last character.
pub fn utf16_to_byte(line: &str, utf16_offset: u32) -> usize {
    let mut seen = 0u32;
    for (byte, ch) in line.char_indices() {
        if seen >= utf16_offset {
            return byte;
        }
        seen += ch.len_utf16() as u32;
    }
    line.len()
}

/// Byte offset into `text` of an LSP position.
pub fn position_to_offset(text: &str, position: Position) -> usize {
    let mut offset = 0usize;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i == position.line as usize {
            let bare = line.trim_end_matches(['\r', '\n']);
            return offset + utf16_to_byte(bare, position.character);
        }
        offset += line.len();
    }
    text.len()
}

/// LSP position of a byte offset into `text`.
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let mut line_start = 0usize;
    let mut line = 0u32;
    for (i, chunk) in text.split_inclusive('\n').enumerate() {
        let end = line_start + chunk.len();
        if offset < end || end == text.len() {
            line = i as u32;
            break;
        }
        line_start = end;
    }
    let bare = text[line_start..].split('\n').next().unwrap_or("");
    let within = offset.saturating_sub(line_start).min(bare.len());
    Position::new(line, byte_to_utf16(bare, within))
}

/// Byte range in `text` for an LSP range.
pub fn range_to_offsets(text: &str, range: Range) -> (usize, usize) {
    let start = position_to_offset(text, range.start);
    let end = position_to_offset(text, range.end);
    (start, end.max(start))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// How serious a diagnostic is. Unknown values become [`Severity::Info`]
/// rather than being dropped.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn from_lsp(value: Option<u64>) -> Self {
        match value {
            Some(1) => Self::Error,
            Some(2) => Self::Warning,
            Some(3) => Self::Info,
            Some(4) => Self::Hint,
            _ => Self::Info,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Hint => "hint",
        }
    }
}

/// One problem the server reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    /// Rule or error code, e.g. `E0308` or `unused_variables`.
    pub code: Option<String>,
    pub message: String,
    /// Which server produced it, when it says.
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            range: serde_json::from_value(value.get("range")?.clone()).ok()?,
            severity: Severity::from_lsp(value.get("severity").and_then(|s| s.as_u64())),
            code: value.get("code").map(|c| match c {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            message: value.get("message")?.as_str()?.to_string(),
            source: value.get("source").and_then(|s| s.as_str()).map(String::from),
        })
    }

    /// One line, the way a compiler would print it.
    pub fn line(&self) -> String {
        let code = self.code.as_deref().map(|c| format!("[{c}] ")).unwrap_or_default();
        format!(
            "{}:{}: {}: {code}{}",
            self.range.start.line + 1,
            self.range.start.character + 1,
            self.severity.label(),
            self.message
        )
    }
}

/// A place in a file: what go-to-definition and find-references return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

impl Location {
    /// Accepts `Location`, `LocationLink`, or a single-element array of
    /// either — servers disagree about which they send.
    pub fn parse_any(value: &serde_json::Value) -> Vec<Self> {
        match value {
            serde_json::Value::Array(items) => {
                items.iter().flat_map(Self::parse_any).collect()
            }
            serde_json::Value::Object(_) => {
                if let (Some(uri), Some(range)) = (
                    value.get("uri").and_then(|u| u.as_str()),
                    value.get("range").cloned(),
                ) {
                    if let Ok(range) = serde_json::from_value(range) {
                        return vec![Self { uri: uri.to_string(), range }];
                    }
                }
                // LocationLink
                if let (Some(uri), Some(range)) = (
                    value.get("targetUri").and_then(|u| u.as_str()),
                    value
                        .get("targetSelectionRange")
                        .or_else(|| value.get("targetRange"))
                        .cloned(),
                ) {
                    if let Ok(range) = serde_json::from_value(range) {
                        return vec![Self { uri: uri.to_string(), range }];
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// One completion candidate, reduced to what the editor inserts and shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    /// Short type/kind hint shown next to the label.
    pub detail: Option<String>,
    /// Text to insert, which is often not the label.
    pub insert: String,
    /// Explicit replacement range, when the server gave one.
    pub range: Option<Range>,
    /// Server-provided ordering key.
    pub sort_text: Option<String>,
    pub kind: Option<u64>,
}

impl CompletionItem {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let label = value.get("label")?.as_str()?.to_string();
        let text_edit = value.get("textEdit");
        let range = text_edit
            .and_then(|e| e.get("range").or_else(|| e.get("replace")))
            .and_then(|r| serde_json::from_value(r.clone()).ok());
        let insert = text_edit
            .and_then(|e| e.get("newText").and_then(|t| t.as_str()))
            .or_else(|| value.get("insertText").and_then(|t| t.as_str()))
            .unwrap_or(&label)
            .to_string();
        Some(Self {
            label,
            detail: value
                .get("detail")
                .and_then(|d| d.as_str())
                .map(String::from)
                .or_else(|| {
                    value.get("labelDetails")?.get("description")?.as_str().map(String::from)
                }),
            insert,
            range,
            sort_text: value.get("sortText").and_then(|s| s.as_str()).map(String::from),
            kind: value.get("kind").and_then(|k| k.as_u64()),
        })
    }

    /// Single-letter kind marker for the popup (f = function, v = variable…).
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            Some(2) | Some(3) => "fn",
            Some(5) => "field",
            Some(6) => "var",
            Some(7) => "type",
            Some(8) => "iface",
            Some(9) => "mod",
            Some(14) => "kw",
            Some(21) => "const",
            Some(22) => "struct",
            _ => "",
        }
    }
}

/// One entry in a file's symbol outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: u64,
    pub range: Range,
    /// Nesting depth, flattened from `DocumentSymbol.children`.
    pub depth: usize,
    pub detail: Option<String>,
}

impl Symbol {
    /// Flattens either response shape: hierarchical `DocumentSymbol[]` or
    /// flat `SymbolInformation[]`.
    pub fn parse_all(value: &serde_json::Value) -> Vec<Self> {
        fn walk(value: &serde_json::Value, depth: usize, out: &mut Vec<Symbol>) {
            let Some(name) = value.get("name").and_then(|n| n.as_str()) else { return };
            let range = value
                .get("selectionRange")
                .or_else(|| value.get("range"))
                .or_else(|| value.pointer("/location/range"))
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_default();
            out.push(Symbol {
                name: name.to_string(),
                kind: value.get("kind").and_then(|k| k.as_u64()).unwrap_or(0),
                range,
                depth,
                detail: value.get("detail").and_then(|d| d.as_str()).map(String::from),
            });
            if let Some(children) = value.get("children").and_then(|c| c.as_array()) {
                for child in children {
                    walk(child, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(items) = value.as_array() {
            for item in items {
                walk(item, 0, &mut out);
            }
        }
        out
    }

    /// Human-readable kind, from the `SymbolKind` enumeration.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            2 => "module",
            5 => "class",
            6 => "method",
            8 => "field",
            10 => "enum",
            11 => "interface",
            12 => "function",
            13 => "variable",
            14 => "constant",
            23 => "struct",
            26 => "type",
            _ => "symbol",
        }
    }
}

/// Plain text of a hover response, whichever shape it arrived in.
pub fn hover_text(value: &serde_json::Value) -> Option<String> {
    fn one(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(_) => value
                .get("value")
                .and_then(|v| v.as_str())
                .map(String::from),
            _ => None,
        }
    }
    let contents = value.get("contents")?;
    let text = match contents {
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(one).collect();
            parts.join("\n\n")
        }
        other => one(other)?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One text replacement, as servers send for formatting and rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    pub fn parse_all(value: &serde_json::Value) -> Vec<Self> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(Self {
                            range: serde_json::from_value(item.get("range")?.clone()).ok()?,
                            new_text: item.get("newText")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Applies edits to `text`, last-first so earlier offsets stay valid.
///
/// Overlapping edits are a server bug; the later one wins rather than
/// producing scrambled text.
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| (e.range.start.line, e.range.start.character));
    let mut out = text.to_string();
    let mut last_start = usize::MAX;
    for edit in sorted.iter().rev() {
        let (start, end) = range_to_offsets(text, edit.range);
        if end > last_start {
            continue; // overlaps an edit already applied
        }
        last_start = start;
        out.replace_range(start..end, &edit.new_text);
    }
    out
}

/// A rename's edits, grouped by file URI.
pub fn workspace_edits(value: &serde_json::Value) -> Vec<(String, Vec<TextEdit>)> {
    let mut out: Vec<(String, Vec<TextEdit>)> = Vec::new();
    if let Some(changes) = value.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            out.push((uri.clone(), TextEdit::parse_all(edits)));
        }
    }
    // documentChanges is the newer shape and takes precedence when both exist.
    if let Some(changes) = value.get("documentChanges").and_then(|c| c.as_array()) {
        out.clear();
        for change in changes {
            let Some(uri) = change.pointer("/textDocument/uri").and_then(|u| u.as_str())
            else {
                continue;
            };
            if let Some(edits) = change.get("edits") {
                out.push((uri.to_string(), TextEdit::parse_all(edits)));
            }
        }
    }
    out.retain(|(_, edits)| !edits.is_empty());
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let message = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "hi"});
        let mut buf = Vec::new();
        write_message(&mut buf, &message).unwrap();
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));

        let mut reader = std::io::BufReader::new(buf.as_slice());
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), message);
        assert!(read_message(&mut reader).unwrap().is_none(), "clean EOF");
    }

    #[test]
    fn content_length_counts_bytes_not_characters() {
        let message = serde_json::json!({"text": "héllo→"});
        let mut buf = Vec::new();
        write_message(&mut buf, &message).unwrap();
        let mut reader = std::io::BufReader::new(buf.as_slice());
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), message);
    }

    #[test]
    fn uris_round_trip_including_spaces() {
        let path = std::path::Path::new("/tmp/my repo/src/main.rs");
        let uri = path_to_uri(path);
        assert!(uri.starts_with("file:///tmp/my%20repo/"), "{uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn utf16_offsets_survive_wide_characters() {
        // "🦀" is two UTF-16 code units and four bytes.
        let line = "let x = \"🦀\"; // tail";
        let byte = line.find("; //").unwrap();
        let utf16 = byte_to_utf16(line, byte);
        assert_eq!(utf16_to_byte(line, utf16), byte);
        assert!(utf16 < byte as u32, "utf16 offset must be shorter than the byte offset");
    }

    #[test]
    fn positions_map_to_offsets_both_ways() {
        let text = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let offset = position_to_offset(text, Position::new(1, 3));
        assert_eq!(&text[offset..offset + 1], "b");
        assert_eq!(offset_to_position(text, offset), Position::new(1, 3));
    }

    #[test]
    fn a_position_past_the_end_clamps() {
        let text = "one\n";
        assert_eq!(position_to_offset(text, Position::new(9, 9)), text.len());
        assert_eq!(utf16_to_byte("ab", 99), 2);
    }

    #[test]
    fn diagnostics_parse_with_and_without_a_code() {
        let value = serde_json::json!({
            "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
            "severity": 1,
            "code": "E0308",
            "message": "mismatched types",
            "source": "rustc"
        });
        let d = Diagnostic::parse(&value).unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.code.as_deref(), Some("E0308"));
        assert!(d.line().starts_with("4:5: error: [E0308] mismatched types"));

        let bare = serde_json::json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "hmm"
        });
        assert_eq!(Diagnostic::parse(&bare).unwrap().severity, Severity::Info);
    }

    #[test]
    fn locations_parse_from_every_shape() {
        let range = serde_json::json!({
            "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}
        });
        let plain = serde_json::json!({"uri": "file:///a.rs", "range": range});
        assert_eq!(Location::parse_any(&plain).len(), 1);

        let link = serde_json::json!({"targetUri": "file:///b.rs", "targetSelectionRange": range});
        assert_eq!(Location::parse_any(&link)[0].uri, "file:///b.rs");

        let array = serde_json::json!([plain, link]);
        assert_eq!(Location::parse_any(&array).len(), 2);
        assert!(Location::parse_any(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn hover_text_handles_markup_and_arrays() {
        let markup = serde_json::json!({"contents": {"kind": "markdown", "value": "`fn go()`"}});
        assert_eq!(hover_text(&markup).unwrap(), "`fn go()`");
        let array = serde_json::json!({"contents": ["one", {"value": "two"}]});
        assert_eq!(hover_text(&array).unwrap(), "one\n\ntwo");
        assert!(hover_text(&serde_json::json!({"contents": ""})).is_none());
    }

    #[test]
    fn edits_apply_back_to_front() {
        let text = "aaa bbb ccc";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 3),
                },
                new_text: "XXXX".into(),
            },
            TextEdit {
                range: Range {
                    start: Position::new(0, 8),
                    end: Position::new(0, 11),
                },
                new_text: "Z".into(),
            },
        ];
        assert_eq!(apply_edits(text, &edits), "XXXX bbb Z");
    }

    #[test]
    fn symbols_flatten_with_depth() {
        let value = serde_json::json!([{
            "name": "App", "kind": 23,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
            "children": [{
                "name": "run", "kind": 6,
                "range": {"start": {"line": 2, "character": 4}, "end": {"line": 4, "character": 4}}
            }]
        }]);
        let symbols = Symbol::parse_all(&value);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[1].name, "run");
        assert_eq!(symbols[1].depth, 1);
        assert_eq!(symbols[0].kind_label(), "struct");
    }

    #[test]
    fn workspace_edits_prefer_document_changes() {
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}
        });
        let value = serde_json::json!({
            "changes": {"file:///old.rs": [{"range": range, "newText": "old"}]},
            "documentChanges": [{
                "textDocument": {"uri": "file:///new.rs", "version": 1},
                "edits": [{"range": range, "newText": "new"}]
            }]
        });
        let grouped = workspace_edits(&value);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, "file:///new.rs");
        assert_eq!(grouped[0].1[0].new_text, "new");
    }
}
