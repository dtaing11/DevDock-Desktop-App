//! A minimal line diff, for showing a proposed change before it is applied.
//!
//! The rest of the app renders diffs that `git` produced. An AI proposal has
//! never been written to disk, so there is nothing for git to diff — this
//! module fills that gap and nothing else.

/// One line of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Context(String),
    Added(String),
    Removed(String),
    /// A run of unchanged lines that was collapsed, with how many.
    Skipped(usize),
}

/// Lines of context kept around each change.
const CONTEXT: usize = 3;

/// Above this many lines on either side, the quadratic match is skipped and
/// the file is shown as a wholesale replacement. Proposals that large are
/// rare, and a UI that freezes is worse than a coarse diff.
const MAX_ALIGNED_LINES: usize = 4_000;

/// Diffs two texts by line, collapsing long unchanged runs.
pub fn diff(before: &str, after: &str) -> Vec<Line> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();

    if a.len() > MAX_ALIGNED_LINES || b.len() > MAX_ALIGNED_LINES {
        let mut out: Vec<Line> = a.iter().map(|l| Line::Removed(l.to_string())).collect();
        out.extend(b.iter().map(|l| Line::Added(l.to_string())));
        return out;
    }

    collapse(&align(&a, &b))
}

/// Longest-common-subsequence alignment, after trimming the shared head and
/// tail so the quadratic part only sees the region that actually differs.
fn align(a: &[&str], b: &[&str]) -> Vec<Line> {
    let head = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();

    let mut out: Vec<Line> = a[..head].iter().map(|l| Line::Context(l.to_string())).collect();

    let mid_a = &a[head..a.len() - tail];
    let mid_b = &b[head..b.len() - tail];

    // lcs[i][j]: length of the LCS of mid_a[i..] and mid_b[j..].
    let (n, m) = (mid_a.len(), mid_b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if mid_a[i] == mid_b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if mid_a[i] == mid_b[j] {
            out.push(Line::Context(mid_a[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(Line::Removed(mid_a[i].to_string()));
            i += 1;
        } else {
            out.push(Line::Added(mid_b[j].to_string()));
            j += 1;
        }
    }
    out.extend(mid_a[i..].iter().map(|l| Line::Removed(l.to_string())));
    out.extend(mid_b[j..].iter().map(|l| Line::Added(l.to_string())));
    out.extend(
        a[a.len() - tail..].iter().map(|l| Line::Context(l.to_string())),
    );
    out
}

/// Replaces long runs of unchanged lines with a [`Line::Skipped`] marker.
fn collapse(lines: &[Line]) -> Vec<Line> {
    let changed: Vec<bool> =
        lines.iter().map(|l| !matches!(l, Line::Context(_))).collect();
    let keep: Vec<bool> = (0..lines.len())
        .map(|i| {
            let lo = i.saturating_sub(CONTEXT);
            let hi = (i + CONTEXT + 1).min(lines.len());
            changed[lo..hi].iter().any(|c| *c)
        })
        .collect();

    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if skipped > 0 {
                out.push(Line::Skipped(skipped));
                skipped = 0;
            }
            out.push(line.clone());
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(Line::Skipped(skipped));
    }
    out
}

/// Added and removed line counts for a rendered diff.
pub fn tally(lines: &[Line]) -> (usize, usize) {
    let count = |f: fn(&Line) -> bool| lines.iter().filter(|l| f(l)).count();
    (
        count(|l| matches!(l, Line::Added(_))),
        count(|l| matches!(l, Line::Removed(_))),
    )
}

// ---------------------------------------------------------------------------
// Word-level diff
// ---------------------------------------------------------------------------

/// Byte ranges within one line, each covering text that changed.
pub type Spans = Vec<(usize, usize)>;

/// Byte ranges within a changed line that actually differ from its partner.
///
/// A unified diff marks a whole line as removed and another as added, even
/// when one identifier changed. Reading it means scanning two nearly
/// identical lines for the difference — which is work a machine should do.
///
/// Both arguments are line *content*: the caller strips the diff's own `+`
/// or `-` marker first, or every pair looks like it changed from the first
/// character.
pub fn changed_words(before: &str, after: &str) -> (Spans, Spans) {
    let a = tokenize(before);
    let b = tokenize(after);

    // Same trick as the line diff: trim the common head and tail, then align
    // only what is left.
    let head = a.iter().zip(b.iter()).take_while(|(x, y)| x.1 == y.1).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x.1 == y.1)
        .count();

    let mid_a = &a[head..a.len() - tail];
    let mid_b = &b[head..b.len() - tail];

    // A line that changed beyond recognition is not helped by highlighting
    // nearly all of it; leave it plain.
    let changed_share = |mid: &[(usize, &str)], all: &[(usize, &str)]| {
        let changed: usize = mid.iter().map(|(_, t)| t.len()).sum();
        let total: usize = all.iter().map(|(_, t)| t.len()).sum::<usize>().max(1);
        changed as f32 / total as f32
    };
    if changed_share(mid_a, &a) > 0.8 && changed_share(mid_b, &b) > 0.8 {
        return (Vec::new(), Vec::new());
    }

    (spans(mid_a, before), spans(mid_b, after))
}

/// Merges a token run into byte ranges, skipping pure whitespace at the
/// edges so the highlight sits on the words and not the gaps.
fn spans(tokens: &[(usize, &str)], line: &str) -> Spans {
    let mut out: Spans = Vec::new();
    for (start, text) in tokens {
        if text.trim().is_empty() {
            continue;
        }
        let range = (*start, start + text.len());
        match out.last_mut() {
            // Join runs separated only by whitespace, so `a  b` highlights
            // as one span rather than two.
            Some(last) if line[last.1..range.0].trim().is_empty() => last.1 = range.1,
            _ => out.push(range),
        }
    }
    out
}

/// Splits a line into words, keeping byte offsets. Identifier characters
/// group together; everything else is its own token, so `foo(bar)` differs
/// from `foo(baz)` by one token rather than by the whole call.
fn tokenize(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let word = c.is_alphanumeric() || c == '_';
        let mut end = start + c.len_utf8();
        if word {
            while let Some((i, next)) = chars.peek().copied() {
                if next.is_alphanumeric() || next == '_' {
                    end = i + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
        }
        out.push((start, &line[start..end]));
    }
    out
}

/// Pairs each removed line in a unified diff with the added line that
/// replaced it, if any.
///
/// Only balanced runs are paired: three removed lines followed by three
/// added ones line up one-to-one, while three removed followed by one added
/// is a rewrite, not an edit, and pairing it would invent changes.
pub fn pair_changed_lines(lines: &[&str]) -> std::collections::HashMap<usize, usize> {
    let mut pairs = std::collections::HashMap::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_removal(lines[i]) {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < lines.len() && is_removal(lines[i]) {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && is_addition(lines[i]) {
            i += 1;
        }
        let (removed, added) = (added_start - removed_start, i - added_start);
        if removed == added {
            for offset in 0..removed {
                pairs.insert(removed_start + offset, added_start + offset);
            }
        }
    }
    pairs
}

fn is_removal(line: &str) -> bool {
    line.starts_with('-') && !line.starts_with("---")
}

fn is_addition(line: &str) -> bool {
    line.starts_with('+') && !line.starts_with("+++")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_single_changed_line() {
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            d,
            vec![
                Line::Context("a".into()),
                Line::Removed("b".into()),
                Line::Added("B".into()),
                Line::Context("c".into()),
            ]
        );
        assert_eq!(tally(&d), (1, 1));
    }

    #[test]
    fn identical_texts_have_no_changes() {
        assert_eq!(tally(&diff("a\nb\n", "a\nb\n")), (0, 0));
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let d = diff("", "a\nb\n");
        assert_eq!(tally(&d), (2, 0));
    }

    #[test]
    fn long_unchanged_runs_collapse() {
        let before: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let after = before.replace("line 20", "line twenty");
        let d = diff(&before, &after);
        assert!(d.iter().any(|l| matches!(l, Line::Skipped(n) if *n > 10)), "{d:?}");
        assert_eq!(tally(&d), (1, 1));
        // Context lines survive around the change.
        assert!(d.contains(&Line::Context("line 19".into())));
    }

    #[test]
    fn insertions_and_deletions_align() {
        let d = diff("a\nb\nc\nd\n", "a\nc\nx\nd\n");
        assert_eq!(tally(&d), (1, 1));
        assert!(d.contains(&Line::Removed("b".into())));
        assert!(d.contains(&Line::Added("x".into())));
    }

    #[test]
    fn huge_files_fall_back_to_wholesale_replacement() {
        let before: String = (0..MAX_ALIGNED_LINES + 1).map(|i| format!("{i}\n")).collect();
        let d = diff(&before, "x\n");
        assert_eq!(tally(&d), (1, MAX_ALIGNED_LINES + 1));
    }

    #[test]
    fn word_diff_marks_only_what_changed() {
        // Content, with the diff's own +/- marker already stripped.
        let before = "    let total = compute(values, 3);";
        let after = "    let total = compute(values, 4);";
        let (removed, added) = changed_words(before, after);
        fn text<'a>(line: &'a str, spans: &[(usize, usize)]) -> Vec<&'a str> {
            spans.iter().map(|(a, b)| &line[*a..*b]).collect()
        }
        assert_eq!(text(before, &removed), vec!["3"]);
        assert_eq!(text(after, &added), vec!["4"]);
    }

    #[test]
    fn word_diff_groups_a_run_of_changes() {
        let before = "let name = old_thing.value();";
        let after = "let name = new_thing.other();";
        let (removed, added) = changed_words(before, after);
        fn text<'a>(line: &'a str, spans: &[(usize, usize)]) -> Vec<&'a str> {
            spans.iter().map(|(a, b)| &line[*a..*b]).collect()
        }
        assert_eq!(text(before, &removed), vec!["old_thing.value"]);
        assert_eq!(text(after, &added), vec!["new_thing.other"]);
    }

    #[test]
    fn a_wholly_rewritten_line_is_not_highlighted() {
        // Marking 90% of a line as changed helps nobody.
        let (removed, added) = changed_words(
            "let a = one();",
            "completely different content here entirely",
        );
        assert!(removed.is_empty() && added.is_empty());
    }

    #[test]
    fn identical_lines_have_nothing_marked() {
        let (removed, added) = changed_words("same line", "same line");
        assert!(removed.is_empty() && added.is_empty());
    }

    #[test]
    fn word_offsets_survive_wide_characters() {
        let before = "let s = \"🦀 old\";";
        let after = "let s = \"🦀 new\";";
        let (removed, added) = changed_words(before, after);
        // Slicing at these offsets must not panic, and must be the change.
        assert_eq!(removed.iter().map(|(a, b)| &before[*a..*b]).collect::<Vec<_>>(), vec!["old"]);
        assert_eq!(added.iter().map(|(a, b)| &after[*a..*b]).collect::<Vec<_>>(), vec!["new"]);
    }

    #[test]
    fn balanced_runs_pair_up_and_unbalanced_ones_do_not() {
        let lines = vec![
            "@@ -1,4 +1,4 @@",
            " context",
            "-old one",
            "-old two",
            "+new one",
            "+new two",
            " context",
            "-rewritten",
            "+a",
            "+b",
            "+c",
        ];
        let pairs = pair_changed_lines(&lines);
        assert_eq!(pairs.get(&2), Some(&4));
        assert_eq!(pairs.get(&3), Some(&5));
        // 1 removed vs 3 added is a rewrite, not an edit.
        assert_eq!(pairs.get(&7), None);
    }

    #[test]
    fn file_headers_are_not_mistaken_for_changed_lines() {
        let lines = vec!["--- a/x.rs", "+++ b/x.rs", "-real removal", "+real addition"];
        let pairs = pair_changed_lines(&lines);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs.get(&2), Some(&3));
    }
}
