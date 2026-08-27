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
}
