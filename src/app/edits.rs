//! Text operations the editor performs on a buffer.
//!
//! Pure functions over `(text, selection)`, returning the new text and where
//! the selection should end up. Keeping them out of the UI is what makes
//! them testable: "move this line up" has a dozen edge cases — the first
//! line, the last line, a selection spanning three lines, a file with no
//! trailing newline — and none of them need a window to check.
//!
//! Offsets are **bytes**, matching the buffer. The UI converts to and from
//! egui's character indices at the boundary.

/// A selection, as a byte range. `start` may be after `end` when the user
/// dragged backwards; every function here normalizes first.
pub type Range = (usize, usize);

fn ordered(sel: Range) -> Range {
    if sel.0 <= sel.1 { sel } else { (sel.1, sel.0) }
}

/// The byte range of the lines a selection touches, including the whole
/// first and last line.
pub fn line_span(text: &str, sel: Range) -> Range {
    let (start, end) = ordered(sel);
    let start = text[..start.min(text.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = text[end.min(text.len())..]
        .find('\n')
        .map(|i| end + i)
        .unwrap_or(text.len());
    (start, end)
}

/// The line comment marker for a language, if it has one.
pub fn line_comment(lang: super::syntax::Lang) -> Option<&'static str> {
    use super::syntax::Lang;
    match lang {
        Lang::Rust | Lang::Java | Lang::Dart | Lang::JavaScript | Lang::Go => Some("//"),
        Lang::Python => Some("#"),
        Lang::Css => None,   // /* */ only
        Lang::Html => None,  // <!-- --> only
        Lang::Plain => Some("#"),
    }
}

/// Comments the selected lines, or uncomments them when every non-blank one
/// is already commented — the behaviour every editor has, and the one that
/// makes a single keystroke a toggle.
pub fn toggle_comment(text: &str, sel: Range, marker: &str) -> (String, Range) {
    let (start, end) = line_span(text, sel);
    let block = &text[start..end];
    let lines: Vec<&str> = block.split('\n').collect();

    let commented = |line: &str| line.trim_start().starts_with(marker);
    let all_commented = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| commented(l));

    // Comment at the shallowest indentation, so a block keeps its shape.
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let rewritten: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return (*line).to_string();
            }
            if all_commented {
                // Remove the marker and one space after it, if present.
                let at = line.find(marker).unwrap_or(0);
                let after = at + marker.len();
                let after = if line[after..].starts_with(' ') { after + 1 } else { after };
                format!("{}{}", &line[..at], &line[after..])
            } else {
                format!("{}{marker} {}", &line[..indent], &line[indent..])
            }
        })
        .collect();

    let replacement = rewritten.join("\n");
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..start]);
    out.push_str(&replacement);
    out.push_str(&text[end..]);
    (out, (start, start + replacement.len()))
}

/// Copies the selected lines below themselves.
pub fn duplicate_lines(text: &str, sel: Range) -> (String, Range) {
    let (start, end) = line_span(text, sel);
    let block = &text[start..end];
    let mut out = String::with_capacity(text.len() + block.len() + 1);
    out.push_str(&text[..end]);
    out.push('\n');
    out.push_str(block);
    out.push_str(&text[end..]);
    // Put the cursor on the copy, which is what you want to edit.
    let new_start = end + 1;
    (out, (new_start, new_start + block.len()))
}

/// Moves the selected lines up or down one line.
///
/// A no-op at the top or bottom rather than an error: holding the shortcut
/// against the edge of the file should do nothing, not lose a line.
pub fn move_lines(text: &str, sel: Range, up: bool) -> (String, Range) {
    let (start, end) = line_span(text, sel);
    if up && start == 0 {
        return (text.to_string(), sel);
    }
    // `end` sits on the newline that ends the block, so a line exists below
    // only if there is content after it. A file ending in a newline has no
    // last line to swap with.
    if !up && end + 1 >= text.len() {
        return (text.to_string(), sel);
    }

    if up {
        let previous_start = text[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let previous = &text[previous_start..start - 1];
        let block = &text[start..end];
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..previous_start]);
        out.push_str(block);
        out.push('\n');
        out.push_str(previous);
        out.push_str(&text[end..]);
        let shift = start - previous_start;
        (out, (start - shift, end - shift))
    } else {
        let next_end = text[end + 1..]
            .find('\n')
            .map(|i| end + 1 + i)
            .unwrap_or(text.len());
        let next = &text[end + 1..next_end];
        let block = &text[start..end];
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..start]);
        out.push_str(next);
        out.push('\n');
        out.push_str(block);
        out.push_str(&text[next_end..]);
        let shift = next.len() + 1;
        (out, (start + shift, end + shift))
    }
}

/// Byte offset of the start of a 1-based line, clamped to the file.
pub fn line_start(text: &str, line: u32) -> usize {
    let target = line.max(1) as usize - 1;
    let mut offset = 0;
    for (i, chunk) in text.split_inclusive('\n').enumerate() {
        if i == target {
            return offset;
        }
        offset += chunk.len();
    }
    text.len()
}

/// 1-based line and column (in characters) of a byte offset.
pub fn line_col(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let line = text[..offset].matches('\n').count() as u32 + 1;
    let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = text[line_start..offset].chars().count() as u32 + 1;
    (line, col)
}

// ---------------------------------------------------------------------------
// Brackets
// ---------------------------------------------------------------------------

/// The bracket pairs the editor knows about.
const PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

/// The quote characters that auto-close.
const QUOTES: [char; 3] = ['"', '\'', '`'];

/// Whether `ch` opens a pair, and what closes it.
pub fn opening(ch: char) -> Option<char> {
    PAIRS.iter().find(|(open, _)| *open == ch).map(|(_, close)| *close)
}

/// Whether `ch` closes a pair, and what opened it.
pub fn closing(ch: char) -> Option<char> {
    PAIRS.iter().find(|(_, close)| *close == ch).map(|(open, _)| *open)
}

/// The offset of the bracket matching the one at or just before `cursor`.
///
/// Scans with a depth counter rather than matching the first candidate, so
/// nesting works, and skips brackets inside strings and comments — a `{` in
/// a string literal is not a brace, and highlighting it as one is worse than
/// highlighting nothing.
pub fn matching_bracket(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let at = |offset: usize| text[offset..].chars().next();
    // The bracket under the cursor, or the one just behind it — both are
    // "the bracket you are on" as far as a person is concerned.
    let candidates = [cursor, cursor.saturating_sub(1)];
    let (start, ch) = candidates.into_iter().find_map(|offset| {
        if offset >= text.len() || !text.is_char_boundary(offset) {
            return None;
        }
        let ch = at(offset)?;
        (opening(ch).is_some() || closing(ch).is_some()).then_some((offset, ch))
    })?;
    if in_string_or_comment(text, start) {
        return None;
    }

    if let Some(close) = opening(ch) {
        let mut depth = 0i32;
        for (offset, c) in text[start..].char_indices() {
            let offset = start + offset;
            if in_string_or_comment(text, offset) {
                continue;
            }
            if c == ch {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some((start, offset));
                }
            }
        }
        return None;
    }

    let open = closing(ch)?;
    let mut depth = 0i32;
    for (offset, c) in text[..=start].char_indices().rev() {
        if in_string_or_comment(text, offset) {
            continue;
        }
        if c == ch {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                return Some((offset, start));
            }
        }
    }
    None
}

/// Whether an offset falls inside a string literal or a line comment.
///
/// A single pass from the start of the line: enough for the brace matching
/// and auto-closing to behave, without pretending to be a parser.
fn in_string_or_comment(text: &str, offset: usize) -> bool {
    let line_start = text[..offset.min(text.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut previous = '\0';
    for (i, ch) in text[line_start..offset.min(text.len())].char_indices() {
        let _ = i;
        if escaped {
            escaped = false;
            previous = ch;
            continue;
        }
        match quote {
            Some(q) => {
                if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
            }
            None => {
                if QUOTES.contains(&ch) {
                    quote = Some(ch);
                } else if (ch == '/' && previous == '/') || ch == '#' {
                    return true;
                }
            }
        }
        previous = ch;
    }
    quote.is_some()
}

/// What typing `ch` should insert, given what follows the cursor.
///
/// Returns the text to insert and where the caret should end up within it.
/// `None` means "nothing special": let the character be typed normally.
pub fn auto_close(text: &str, cursor: usize, ch: char) -> Option<(String, usize)> {
    let next = text[cursor.min(text.len())..].chars().next();

    // Typing the closing character when it is already there just steps over
    // it, which is what makes auto-closing bearable.
    if next == Some(ch) && (closing(ch).is_some() || QUOTES.contains(&ch)) {
        return Some((String::new(), 1));
    }
    // Only close before whitespace or a closing bracket; typing `(` in the
    // middle of a word means wrapping, not opening a pair.
    let closes_here = match next {
        None => true,
        Some(c) => c.is_whitespace() || closing(c).is_some() || c == ',' || c == ';',
    };
    if !closes_here {
        return None;
    }
    if let Some(close) = opening(ch) {
        // `<` is a pair in generics and a comparison everywhere else; not
        // worth guessing wrong on.
        if ch == '<' {
            return None;
        }
        return Some((format!("{ch}{close}"), 1));
    }
    if QUOTES.contains(&ch) && !in_string_or_comment(text, cursor) {
        return Some((format!("{ch}{ch}"), 1));
    }
    None
}

/// The indentation a new line should start with, given the line before it.
pub fn auto_indent(text: &str, cursor: usize) -> String {
    let line_start = text[..cursor.min(text.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line = &text[line_start..cursor.min(text.len())];
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    // One level deeper after an opening brace, which is the only heuristic
    // that earns its keep across languages.
    if line.trim_end().ends_with(['{', '(', '[', ':']) {
        format!("{indent}    ")
    } else {
        indent
    }
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

/// A foldable region: the line that starts it, and the last line inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    /// 0-based line the fold starts on; it stays visible when folded.
    pub start: u32,
    /// 0-based last line hidden by the fold.
    pub end: u32,
}

/// Foldable regions, from indentation.
///
/// Indentation rather than syntax: it works in every language the editor
/// opens, including the ones with no language server, and it agrees with
/// what a reader sees. A blank line does not end a region — code is full of
/// them — but a line at or below the opening indentation does.
pub fn folds(text: &str) -> Vec<Fold> {
    let lines: Vec<&str> = text.lines().collect();
    let indent_of = |line: &str| -> Option<usize> {
        (!line.trim().is_empty()).then(|| line.len() - line.trim_start().len())
    };

    let mut folds = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(indent) = indent_of(line) else { continue };
        // Where does the block under this line end?
        let mut last = i;
        for (j, candidate) in lines.iter().enumerate().skip(i + 1) {
            match indent_of(candidate) {
                Some(other) if other > indent => last = j,
                Some(_) => break,
                // Blank lines belong to the block only if something deeper
                // follows them.
                None => continue,
            }
        }
        if last > i {
            folds.push(Fold { start: i as u32, end: last as u32 });
        }
    }
    folds
}

/// The fold starting on `line`, if any.
pub fn fold_at(folds: &[Fold], line: u32) -> Option<Fold> {
    folds.iter().find(|f| f.start == line).copied()
}

/// Whether a line is hidden by any of the collapsed folds.
pub fn is_hidden(folds: &[Fold], collapsed: &std::collections::HashSet<u32>, line: u32) -> bool {
    folds
        .iter()
        .any(|f| collapsed.contains(&f.start) && line > f.start && line <= f.end)
}

/// How a search matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MatchOptions {
    pub case_sensitive: bool,
    /// Only match whole words, the way `\b…\b` would.
    pub whole_word: bool,
}

/// Every match of `needle` in `text`, as byte ranges.
pub fn find_all(text: &str, needle: &str, options: MatchOptions) -> Vec<Range> {
    if needle.is_empty() {
        return Vec::new();
    }
    let (haystack, needle_owned);
    let needle = if options.case_sensitive {
        haystack = std::borrow::Cow::Borrowed(text);
        needle
    } else {
        haystack = std::borrow::Cow::Owned(text.to_lowercase());
        needle_owned = needle.to_lowercase();
        &needle_owned
    };

    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = haystack[at..].find(needle) {
        let start = at + found;
        let end = start + needle.len();
        at = end.max(start + 1);
        if options.whole_word {
            let before = haystack[..start].chars().next_back();
            let after = haystack[end..].chars().next();
            if before.is_some_and(is_word) || after.is_some_and(is_word) {
                continue;
            }
        }
        // Lower-casing can change byte lengths for some scripts; skip a
        // match whose range does not line up with the original text.
        if text.is_char_boundary(start) && text.is_char_boundary(end) {
            out.push((start, end));
        }
    }
    out
}

/// Replaces every match, returning the new text and how many were replaced.
pub fn replace_all(
    text: &str,
    needle: &str,
    replacement: &str,
    options: MatchOptions,
) -> (String, usize) {
    let matches = find_all(text, needle, options);
    if matches.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in &matches {
        out.push_str(&text[at..*start]);
        out.push_str(replacement);
        at = *end;
    }
    out.push_str(&text[at..]);
    (out, matches.len())
}

/// Replaces one match, given its range.
pub fn replace_at(text: &str, range: Range, replacement: &str) -> String {
    let (start, end) = ordered(range);
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start.min(text.len())]);
    out.push_str(replacement);
    out.push_str(&text[end.min(text.len())..]);
    out
}

/// The match at or after `from`, wrapping to the start.
pub fn next_match(matches: &[Range], from: usize) -> Option<usize> {
    matches
        .iter()
        .position(|(start, _)| *start >= from)
        .or(if matches.is_empty() { None } else { Some(0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_toggles_a_block_at_its_shallowest_indent() {
        let text = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        let sel = (text.find("let x").unwrap(), text.find("let y").unwrap());
        let (commented, _) = toggle_comment(text, sel, "//");
        assert_eq!(
            commented,
            "fn a() {\n    // let x = 1;\n    // let y = 2;\n}\n"
        );

        // Toggling again restores it exactly.
        let sel = (
            commented.find("// let x").unwrap(),
            commented.find("// let y").unwrap(),
        );
        let (back, _) = toggle_comment(&commented, sel, "//");
        assert_eq!(back, text);
    }

    #[test]
    fn a_partly_commented_block_becomes_fully_commented() {
        let text = "// one\ntwo\n";
        let (out, _) = toggle_comment(text, (0, text.len()), "//");
        assert_eq!(out, "// // one\n// two\n");
    }

    #[test]
    fn blank_lines_are_left_alone_when_commenting() {
        let text = "one\n\ntwo\n";
        let (out, _) = toggle_comment(text, (0, text.len()), "#");
        assert_eq!(out, "# one\n\n# two\n");
    }

    #[test]
    fn duplicate_copies_the_line_below_and_selects_the_copy() {
        let text = "one\ntwo\nthree\n";
        let at = text.find("two").unwrap();
        let (out, sel) = duplicate_lines(text, (at, at));
        assert_eq!(out, "one\ntwo\ntwo\nthree\n");
        assert_eq!(&out[sel.0..sel.1], "two");
        // The *second* "two" is the one selected.
        assert!(sel.0 > text.find("two").unwrap());
    }

    #[test]
    fn move_line_down_then_up_is_a_round_trip() {
        let text = "one\ntwo\nthree\n";
        let at = text.find("one").unwrap();
        let (down, sel) = move_lines(text, (at, at), false);
        assert_eq!(down, "two\none\nthree\n");

        let (up, _) = move_lines(&down, sel, true);
        assert_eq!(up, text);
    }

    #[test]
    fn moving_past_the_edges_does_nothing() {
        let text = "one\ntwo\n";
        let (out, sel) = move_lines(text, (0, 0), true);
        assert_eq!(out, text);
        assert_eq!(sel, (0, 0));

        // The last line, with a trailing newline, has nothing below it.
        let last = text.rfind("two").unwrap();
        let (out, _) = move_lines(text, (last, last), false);
        assert_eq!(out, text);
    }

    #[test]
    fn moving_a_multi_line_selection_moves_all_of_it() {
        let text = "a\nb\nc\nd\n";
        let sel = (text.find('b').unwrap(), text.find('c').unwrap());
        let (out, sel) = move_lines(text, sel, false);
        assert_eq!(out, "a\nd\nb\nc\n");
        assert_eq!(&out[sel.0..sel.1], "b\nc");
    }

    #[test]
    fn line_offsets_and_positions_agree() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(line_start(text, 2), 4);
        assert_eq!(&text[line_start(text, 3)..line_start(text, 3) + 5], "three");
        assert_eq!(line_col(text, 4), (2, 1));
        assert_eq!(line_col(text, 6), (2, 3));
        // Past the end clamps rather than panicking.
        assert_eq!(line_start(text, 99), text.len());
    }

    #[test]
    fn find_is_case_insensitive_by_default_and_can_be_strict() {
        let text = "Value value VALUE";
        assert_eq!(find_all(text, "value", MatchOptions::default()).len(), 3);
        let strict = MatchOptions { case_sensitive: true, whole_word: false };
        assert_eq!(find_all(text, "value", strict).len(), 1);
    }

    #[test]
    fn whole_word_search_skips_substrings() {
        let text = "value valueOf my_value value";
        let options = MatchOptions { case_sensitive: true, whole_word: true };
        let hits = find_all(text, "value", options);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(&text[hits[0].0..hits[0].1], "value");
        assert_eq!(hits[1].0, text.rfind("value").unwrap());
    }

    #[test]
    fn overlapping_matches_do_not_loop_forever() {
        let text = "aaaa";
        let hits = find_all(text, "aa", MatchOptions { case_sensitive: true, whole_word: false });
        assert_eq!(hits.len(), 2, "{hits:?}");
    }

    #[test]
    fn replace_all_reports_how_many_it_changed() {
        let text = "one two one";
        let (out, count) = replace_all(text, "one", "1", MatchOptions::default());
        assert_eq!(out, "1 two 1");
        assert_eq!(count, 2);

        let (same, count) = replace_all(text, "zzz", "!", MatchOptions::default());
        assert_eq!(same, text);
        assert_eq!(count, 0);
    }

    #[test]
    fn replace_at_replaces_only_that_match() {
        let text = "one one one";
        let hits = find_all(text, "one", MatchOptions::default());
        let out = replace_at(text, hits[1], "TWO");
        assert_eq!(out, "one TWO one");
    }

    #[test]
    fn next_match_wraps_around() {
        let matches = vec![(2, 5), (10, 13)];
        assert_eq!(next_match(&matches, 0), Some(0));
        assert_eq!(next_match(&matches, 6), Some(1));
        // Past the last match, back to the first.
        assert_eq!(next_match(&matches, 20), Some(0));
        assert_eq!(next_match(&[], 0), None);
    }

    #[test]
    fn wide_characters_do_not_break_offsets() {
        let text = "let s = \"🦀\";\nlet t = \"🦀\";\n";
        let hits = find_all(text, "🦀", MatchOptions::default());
        assert_eq!(hits.len(), 2);
        for (start, end) in hits {
            assert_eq!(&text[start..end], "🦀");
        }
        let (out, _) = duplicate_lines(text, (0, 0));
        assert!(out.starts_with("let s = \"🦀\";\nlet s = \"🦀\";\n"));
    }

    #[test]
    fn brackets_match_across_nesting() {
        let text = "fn a() { if b() { c(); } }";
        let open = text.find('{').unwrap();
        let (start, end) = matching_bracket(text, open).unwrap();
        assert_eq!(start, open);
        assert_eq!(end, text.rfind('}').unwrap());

        // From the closing side too.
        let (start, end) = matching_bracket(text, text.rfind('}').unwrap()).unwrap();
        assert_eq!(start, open);
        assert_eq!(end, text.rfind('}').unwrap());
    }

    #[test]
    fn a_bracket_in_a_string_is_not_a_bracket() {
        let text = "let s = \"{\"; let t = 1;";
        assert!(matching_bracket(text, text.find('{').unwrap()).is_none());
    }

    #[test]
    fn an_unmatched_bracket_matches_nothing() {
        assert!(matching_bracket("fn a() {", 7).is_none());
        assert!(matching_bracket("plain text", 3).is_none());
    }

    #[test]
    fn auto_close_inserts_a_pair_only_where_it_makes_sense() {
        // At the end of a line, or before whitespace.
        assert_eq!(auto_close("let x = ", 8, '('), Some(("()".into(), 1)));
        assert_eq!(auto_close("let x = ;", 8, '('), Some(("()".into(), 1)));
        // In the middle of a word, typing `(` means wrapping.
        assert_eq!(auto_close("value", 2, '('), None);
        // `<` is a comparison as often as a generic.
        assert_eq!(auto_close("a ", 2, '<'), None);
    }

    #[test]
    fn typing_the_closing_character_steps_over_it() {
        // The behaviour that makes auto-closing tolerable.
        assert_eq!(auto_close("()", 1, ')'), Some((String::new(), 1)));
        assert_eq!(auto_close("\"\"", 1, '"'), Some((String::new(), 1)));
    }

    #[test]
    fn quotes_do_not_auto_close_inside_a_string() {
        let text = "let s = \"already open";
        assert_eq!(auto_close(text, text.len(), '"'), None);
    }

    #[test]
    fn auto_indent_follows_the_line_and_opens_a_level() {
        let text = "    let x = 1;";
        assert_eq!(auto_indent(text, text.len()), "    ");
        let text = "    fn a() {";
        assert_eq!(auto_indent(text, text.len()), "        ");
        assert_eq!(auto_indent("no indent", 9), "");
    }

    #[test]
    fn folds_come_from_indentation_and_survive_blank_lines() {
        let text = "fn a() {\n    one();\n\n    two();\n}\n\nfn b() {\n    three();\n}\n";
        let folds = folds(text);
        let first = fold_at(&folds, 0).expect("a fold on the first line");
        // Lines 1..3 are inside it; the blank line does not end the block.
        assert_eq!(first.end, 3);

        let second = fold_at(&folds, 6).expect("a fold on fn b");
        assert_eq!(second.end, 7);
        assert!(fold_at(&folds, 4).is_none(), "a closing brace opens nothing");
    }

    #[test]
    fn collapsing_a_fold_hides_its_body_and_nothing_else() {
        let text = "fn a() {\n    one();\n}\nfn b() {}\n";
        let folds = folds(text);
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert(0);
        assert!(!is_hidden(&folds, &collapsed, 0), "the fold's own line stays visible");
        assert!(is_hidden(&folds, &collapsed, 1));
        assert!(!is_hidden(&folds, &collapsed, 2), "the closing brace is not inside");
        assert!(!is_hidden(&folds, &collapsed, 3));
    }
}
