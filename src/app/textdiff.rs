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
}
