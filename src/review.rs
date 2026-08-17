//! AI code review gate: inspects the work about to be pushed or opened as a
//! pull request, configured per-repository under `[review]` in
//! `.git-manage-ci.toml`.
//!
//! ```toml
//! [review]
//! run = true               # review before every push and pull request
//! block_on_failure = true  # findings at or above `fail_on` cancel it
//! fail_on = "high"         # low | medium | high
//! # provider = "claude"    # claude | ollama; defaults to the app's selection
//! # model = "claude-opus-5"
//! # max_diff_bytes = 24000
//! # instructions = "Flag any new blocking call on the UI thread."
//! ```
//!
//! The model is asked to report *every* finding with a severity, and the
//! `fail_on` threshold decides what blocks. Filtering here rather than in the
//! prompt is deliberate: a model told to report only severe issues
//! investigates just as hard and then withholds the rest, which reads back as
//! a clean review of code that isn't clean.
//!
//! This module owns the config, the prompt, and the parsing. It deliberately
//! knows nothing about the providers — [`crate::claude`] and
//! [`crate::ollama`] each send the prompt, and the app layer picks between
//! them, the same way commit-message generation works.

use serde::{Deserialize, Serialize};

/// How serious a finding is. Ordered, so `>=` implements the `fail_on` gate.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    /// Maps model-supplied severity text onto the three levels, tolerating
    /// the neighbouring words models reach for ("critical", "nit", "info").
    /// Unrecognized text becomes [`Severity::Medium`] rather than being
    /// dropped — an unparsed finding is worse than a mis-ranked one.
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "high" | "critical" | "blocker" | "severe" | "error" => Self::High,
            "low" | "nit" | "info" | "minor" | "suggestion" | "style" => Self::Low,
            _ => Self::Medium,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// One issue the reviewer reported.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Finding {
    /// Repo-relative path, as the model reported it. May be empty when the
    /// finding is about the change as a whole.
    pub file: String,
    pub line: Option<u32>,
    pub severity: Severity,
    /// One-line statement of the problem.
    pub title: String,
    /// Explanation, and a fix where the model offered one.
    pub detail: String,
}

/// The result of one review pass.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ReviewOutcome {
    /// One or two sentences on the overall state of the change.
    pub summary: String,
    /// The reviewer's own account of what it checked and why the findings
    /// matter. Shown alongside the findings so the decision to override is
    /// made against the argument, not just a verdict.
    pub reasoning: String,
    pub findings: Vec<Finding>,
}

impl ReviewOutcome {
    /// Findings at or above `fail_on`, highest severity first.
    pub fn blocking(&self, fail_on: Severity) -> Vec<&Finding> {
        let mut hits: Vec<&Finding> =
            self.findings.iter().filter(|f| f.severity >= fail_on).collect();
        hits.sort_by_key(|f| std::cmp::Reverse(f.severity));
        hits
    }

    /// How many findings sit at each level, as `(high, medium, low)`.
    pub fn tally(&self) -> (usize, usize, usize) {
        let count = |s: Severity| self.findings.iter().filter(|f| f.severity == s).count();
        (count(Severity::High), count(Severity::Medium), count(Severity::Low))
    }
}

/// `[review]` settings from `.git-manage-ci.toml`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReviewConfig {
    /// Run the reviewer before pushes and pull requests.
    #[serde(default)]
    pub run: bool,
    /// When true, findings at or above [`Self::fail_on`] cancel the push or
    /// pull request. When false, they are reported and the action proceeds.
    #[serde(default = "default_true")]
    pub block_on_failure: bool,
    /// Lowest severity that counts as a failure.
    #[serde(default = "default_fail_on")]
    pub fail_on: Severity,
    /// Provider override: `"claude"` or `"ollama"`. Defaults to whatever the
    /// app has selected for AI features.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model override for the chosen provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Cap on how much diff is sent to the model.
    #[serde(default = "default_max_diff")]
    pub max_diff_bytes: usize,
    /// Extra project-specific guidance appended to the prompt.
    #[serde(default)]
    pub instructions: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Only high-severity findings block by default: a gate that fires on
/// style opinions gets switched off, and then nothing is reviewed at all.
fn default_fail_on() -> Severity {
    Severity::High
}

fn default_max_diff() -> usize {
    24_000
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            run: false,
            block_on_failure: true,
            fail_on: default_fail_on(),
            max_diff_bytes: default_max_diff(),
            provider: None,
            model: None,
            instructions: None,
        }
    }
}

/// Instructions for the reviewing model.
pub const SYSTEM_PROMPT: &str = r#"You are reviewing a git diff before it is pushed or opened as a pull request. Report defects in the changed code.

Output ONLY a JSON object, no markdown fences and no prose around it:

{"summary": "one or two sentences on the overall state of the change",
 "reasoning": "what you examined, what you are confident is correct, what you could not verify from the diff alone, and why the findings you report matter. The developer reads this to decide whether to act on your findings or proceed anyway, so give them the argument, not just a verdict. Be honest about uncertainty.",
 "findings": [
   {"file": "src/foo.rs", "line": 42, "severity": "high",
    "title": "one-line statement of the defect",
    "detail": "why it is wrong, the input or state that triggers it, and the fix"}
 ]}

Severity means:
- "high": the change is incorrect or unsafe. Wrong results, crashes, data loss, races, resource leaks, injection, auth or secret exposure, a broken API contract.
- "medium": a real problem that is not a correctness failure. Missing error handling on a path that can fail, a missing test for new branching logic, a performance cliff, a misleading name or comment that will cause a future bug.
- "low": style, naming, formatting, and preference.

Rules:
- Report EVERY finding you have, at its honest severity, including ones you are unsure about. Do not filter for importance — the caller has a configured threshold and decides what blocks. Withholding a finding because it seems minor defeats that.
- Judge only the changed lines and code they directly affect. Do not report pre-existing issues in untouched code.
- Set "file" to the repo-relative path from the diff, and "line" to the line in the new file when you can identify it; use null when you cannot.
- Every finding needs a concrete failing case in "detail": the input, state, or sequence that produces the bad outcome. If you cannot name one, the finding is speculation — either lower its severity or drop it.
- Do not restate what the diff does, praise it, or suggest unrelated refactors.
- An empty "findings" array is the correct answer for a clean change. Do not invent findings to appear thorough."#;

/// Builds the user turn: the diff, plus any project-specific guidance.
pub fn user_prompt(diff: &str, instructions: Option<&str>, max_diff_bytes: usize) -> String {
    let diff = truncate_utf8(diff, max_diff_bytes);
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => {
            format!("Project-specific review instructions:\n{extra}\n\nDiff under review:\n\n{diff}")
        }
        None => format!("Diff under review:\n\n{diff}"),
    }
}

/// Truncates to at most `max` bytes on a char boundary, marking the cut so
/// the model knows it is seeing part of a change.
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[diff truncated — review only what is shown]", &s[..end])
}

/// Shape accepted from the model, before normalization. Every field is
/// optional so one malformed finding cannot discard the whole review.
#[derive(Deserialize, Default)]
struct RawReview {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    findings: Vec<RawFinding>,
}

#[derive(Deserialize)]
struct RawFinding {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: String,
}

/// Extracts a [`ReviewOutcome`] from model output, tolerating markdown fences
/// and prose around the JSON.
///
/// A response that cannot be parsed at all yields an outcome with no findings
/// and the raw text as the summary. That is reported to the user as an
/// inconclusive review; it must never read as "no problems found", so callers
/// check [`ReviewOutcome::findings`] against [`parsed_cleanly`].
pub fn parse(text: &str) -> ReviewOutcome {
    let raw = parse_raw(text).unwrap_or_else(|| RawReview {
        summary: "Could not parse the reviewer's response.".to_string(),
        reasoning: text.trim().to_string(),
        findings: Vec::new(),
    });

    let findings = raw
        .findings
        .into_iter()
        // A finding with neither a title nor a detail says nothing.
        .filter(|f| !f.title.trim().is_empty() || !f.detail.trim().is_empty())
        .map(|f| Finding {
            file: f.file.trim().to_string(),
            line: f.line,
            severity: Severity::parse(&f.severity),
            title: if f.title.trim().is_empty() {
                first_line(&f.detail)
            } else {
                f.title.trim().to_string()
            },
            detail: f.detail.trim().to_string(),
        })
        .collect();

    ReviewOutcome {
        summary: raw.summary.trim().to_string(),
        reasoning: raw.reasoning.trim().to_string(),
        findings,
    }
}

/// Whether `text` held a JSON review at all, as opposed to falling back.
/// A failed parse is an error to surface, not a clean bill of health.
pub fn parsed_cleanly(text: &str) -> bool {
    parse_raw(text).is_some()
}

fn parse_raw(text: &str) -> Option<RawReview> {
    if let Ok(r) = serde_json::from_str::<RawReview>(text.trim()) {
        return Some(r);
    }
    // Models fence the JSON or wrap it in a sentence; take the outermost
    // braces and retry.
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    if end <= start {
        return None;
    }
    let body = &text[start..=end];
    if let Ok(r) = serde_json::from_str::<RawReview>(body) {
        return Some(r);
    }
    // Models routinely hard-wrap long explanations, putting real newlines
    // inside string values — which is invalid JSON. Escaping them recovers
    // an otherwise well-formed review instead of discarding every finding.
    serde_json::from_str::<RawReview>(&escape_newlines_in_strings(body)).ok()
}

/// Escapes raw control characters that appear *inside* JSON string values,
/// leaving the structural whitespace between tokens alone.
fn escape_newlines_in_strings(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in json.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                out.push(c);
                escaped = true;
            }
            '"' => {
                in_string = !in_string;
                out.push(c);
            }
            '\n' if in_string => out.push_str("\\n"),
            '\r' if in_string => out.push_str("\\r"),
            '\t' if in_string => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn first_line(s: &str) -> String {
    s.trim().lines().next().unwrap_or_default().trim().to_string()
}

/// A short, single-line excerpt of an unusable response, for error messages.
pub fn excerpt(text: &str) -> String {
    let line = first_line(text);
    if line.is_empty() {
        return "(empty response)".to_string();
    }
    truncate_utf8(&line, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let out = parse(
            r#"{"summary":"one bug","findings":[
                {"file":"src/a.rs","line":7,"severity":"high",
                 "title":"off-by-one","detail":"len() instead of len()-1"}]}"#,
        );
        assert_eq!(out.summary, "one bug");
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].severity, Severity::High);
        assert_eq!(out.findings[0].line, Some(7));
    }

    #[test]
    fn parses_fenced_json_with_surrounding_prose() {
        let out = parse(
            "Here is my review:\n```json\n{\"summary\":\"ok\",\"findings\":[]}\n```\nHope that helps!",
        );
        assert_eq!(out.summary, "ok");
        assert!(out.findings.is_empty());
        assert!(parsed_cleanly("{\"findings\":[]}"));
    }

    /// An unparseable response must not look like a clean review.
    #[test]
    fn unparseable_response_is_flagged_not_treated_as_clean() {
        let text = "I was unable to review this change.";
        let out = parse(text);
        assert!(out.findings.is_empty());
        assert!(!parsed_cleanly(text), "garbage must not count as a parsed review");
        assert!(out.summary.contains("Could not parse"));
        // The raw text is kept so the user can see what came back.
        assert!(out.reasoning.contains("unable to review"));
    }

    #[test]
    fn captures_reasoning_alongside_findings() {
        let out = parse(
            r#"{"summary":"one issue","reasoning":"I traced the retry path and the
                counter is never reset, so the third call sees a stale value.",
                "findings":[{"severity":"high","title":"stale counter","detail":"d"}]}"#,
        );
        assert!(out.reasoning.contains("never reset"));
        assert_eq!(out.findings.len(), 1);
    }

    #[test]
    fn severity_words_map_onto_three_levels() {
        assert_eq!(Severity::parse("CRITICAL"), Severity::High);
        assert_eq!(Severity::parse("nit"), Severity::Low);
        assert_eq!(Severity::parse("medium"), Severity::Medium);
        // Unknown text is kept as a finding rather than dropped.
        assert_eq!(Severity::parse("spicy"), Severity::Medium);
    }

    #[test]
    fn fail_on_threshold_selects_blocking_findings() {
        let out = parse(
            r#"{"findings":[
                {"severity":"low","title":"naming","detail":"d"},
                {"severity":"medium","title":"no test","detail":"d"},
                {"severity":"high","title":"panic","detail":"d"}]}"#,
        );
        assert_eq!(out.tally(), (1, 1, 1));
        assert_eq!(out.blocking(Severity::High).len(), 1);
        assert_eq!(out.blocking(Severity::Medium).len(), 2);
        assert_eq!(out.blocking(Severity::Low).len(), 3);
    }

    #[test]
    fn findings_without_title_or_detail_are_dropped() {
        let out = parse(r#"{"findings":[{"severity":"high"},{"severity":"low","detail":"real"}]}"#);
        assert_eq!(out.findings.len(), 1);
        // A missing title falls back to the detail's first line.
        assert_eq!(out.findings[0].title, "real");
    }

    #[test]
    fn diff_is_truncated_with_a_marker() {
        let prompt = user_prompt(&"x".repeat(100), None, 20);
        assert!(prompt.contains("[diff truncated"));
    }

    #[test]
    fn default_config_is_off_and_blocks_only_on_high() {
        let c = ReviewConfig::default();
        assert!(!c.run, "review must be opt-in");
        assert!(c.block_on_failure);
        assert_eq!(c.fail_on, Severity::High);
    }
}
