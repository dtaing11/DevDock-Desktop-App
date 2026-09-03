//! AI code review gate: inspects the work about to be pushed or opened as a
//! pull request, configured per-repository under `[review]` in
//! `.git-manage-ci.toml`.
//!
//! ```toml
//! [review]
//! run = true               # review before every push AND pull request
//! block_on_failure = true  # findings at or above `fail_on` cancel it
//! fail_on = "high"         # low | medium | high
//! # provider = "claude"    # claude | ollama; defaults to the app's selection
//! # model = "claude-opus-5"
//! # max_diff_bytes = 24000
//! # instructions = "Flag any new blocking call on the UI thread."
//! ```
//!
//! # Triggers
//!
//! `run` is the both-triggers shorthand. To review one and not the other, set
//! the trigger directly — each falls back to `run` when unset, so existing
//! configs keep working:
//!
//! ```toml
//! [review]
//! on_pull_request = true   # review PRs only; pushes go straight through
//! ```
//!
//! ```toml
//! [review]
//! run = true
//! on_push = false          # an explicit false overrides `run`
//! ```
//!
//! `on_pr` is accepted as a spelling of `on_pull_request`. A manual review
//! from the Checks tab ignores all of this — asking for one is its own
//! consent. See [`ReviewConfig::runs_on_push`] and
//! [`ReviewConfig::runs_on_pull_request`].
//!
//! The model is asked to report *every* finding with a severity, and the
//! `fail_on` threshold decides what blocks. Filtering here rather than in the
//! prompt is deliberate: a model told to report only severe issues
//! investigates just as hard and then withholds the rest, which reads back as
//! a clean review of code that isn't clean.
//!
//! # Custom output
//!
//! `output = "markdown"` swaps the structured contract for the project's own
//! format, written to `output_instructions` and rendered as Markdown by
//! [`crate::app::markdown`]:
//!
//! ```toml
//! [review]
//! run = true
//! output = "markdown"
//! output_instructions = """
//! ## Verdict
//! One line.
//! ## Must fix
//! ## Nits
//! """
//! ```
//!
//! There are no severities in that mode, so `fail_on` does not apply. When
//! `block_on_failure = true` the model is additionally required to lead with a
//! [`VERDICT_PREFIX`] line, which is parsed out and drives the gate before the
//! body is rendered. [`ReviewOutcome::should_block`] hides that difference from
//! callers.
//!
//! This module owns the config, the prompt, and the parsing. It deliberately
//! knows nothing about the providers — [`crate::claude`] and
//! [`crate::ollama`] each send the prompt, and the app layer picks between
//! them, the same way commit-message generation works.

use serde::{Deserialize, Serialize};
use std::path::Path;

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

/// What shape the reviewer should answer in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputStyle {
    /// Structured findings with severities. The `fail_on` threshold applies,
    /// and the app renders the list itself.
    #[default]
    Findings,
    /// Free-form Markdown written to the project's own house style, rendered
    /// as Markdown in the app. There are no severities to threshold, so
    /// blocking relies on a verdict line — see [`VERDICT_PREFIX`].
    Markdown,
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
    /// A verbatim line the reviewer read that shows the problem, and which
    /// is checked against the file before the finding is shown. Empty when
    /// the reviewer offered none.
    #[serde(default)]
    pub evidence: String,
    /// Whether a verifier looked at this finding and stood by it.
    ///
    /// False also means "never examined": a finding the verifier ran out of
    /// budget before reaching is kept, because silence is not a verdict — but
    /// it has not been checked, and a reader deciding whether to override a
    /// gate deserves to know which of the two it is.
    #[serde(default)]
    pub verified: bool,
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
    /// Structured findings. Empty in [`OutputStyle::Markdown`] mode.
    pub findings: Vec<Finding>,
    /// The review body in [`OutputStyle::Markdown`] mode, rendered as
    /// Markdown by the app. `None` in findings mode.
    pub markdown: Option<String>,
    /// Whether the reviewer itself asked to hold the action. Only meaningful
    /// in Markdown mode, where there are no severities to threshold.
    pub verdict_blocks: bool,
    /// What the reviewer looked at: one line per file read or search run.
    /// Empty for a diff-only review. Shown with the findings so a verdict
    /// can be weighed against the context it was reached from.
    #[serde(default)]
    pub context_log: Vec<String>,
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

    /// Whether this review should hold the push or pull request, for either
    /// output style. Keeps the two gating rules in one place so callers do
    /// not have to know which mode produced the outcome.
    pub fn should_block(&self, config: &ReviewConfig) -> bool {
        if !config.block_on_failure {
            return false;
        }
        match config.output {
            OutputStyle::Findings => !self.blocking(config.fail_on).is_empty(),
            // Custom Markdown has no severities to threshold, so the
            // reviewer's own verdict line is the only signal available.
            OutputStyle::Markdown => self.verdict_blocks,
        }
    }
}

/// `[review]` settings from `.git-manage-ci.toml`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReviewConfig {
    /// Run the reviewer before **both** pushes and pull requests. The simple
    /// switch; use [`Self::on_push`] / [`Self::on_pull_request`] to gate one
    /// trigger without the other.
    #[serde(default)]
    pub run: bool,
    /// Review before a push. Falls back to [`Self::run`] when unset, so
    /// `run = true` alone still covers pushes.
    #[serde(default)]
    pub on_push: Option<bool>,
    /// Review before creating a pull request. Falls back to [`Self::run`]
    /// when unset. `on_pr` is accepted as a spelling of this.
    #[serde(default, alias = "on_pr")]
    pub on_pull_request: Option<bool>,
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
    /// Whether the reviewer may read the repository while it reviews.
    ///
    /// On by default. A diff alone cannot show whether the caller three
    /// files over still holds, so the reviewer gets read-only access to the
    /// tracked files ([`crate::agent`]) and decides for itself what to open.
    /// Set `repo_context = false` for a diff-only review.
    #[serde(default = "default_true")]
    pub repo_context: bool,
    /// Cap on how many files the reviewer may open in one review.
    #[serde(default = "default_context_calls")]
    pub max_context_calls: usize,
    /// Cap on how much it may read in total, in bytes.
    #[serde(default = "default_context_bytes")]
    pub max_context_bytes: usize,
    /// Check every finding against the code before showing it.
    ///
    /// Costs one extra request per review with findings, and is the
    /// difference between a gate you trust and one you learn to click past.
    #[serde(default = "default_true")]
    pub verify_findings: bool,
    /// Extra project-specific guidance appended to the prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    /// A file holding the review guidance, as a path relative to the
    /// repository root. For anything longer than a few lines this beats a
    /// TOML multi-line string: it can be edited, reviewed, and diffed like
    /// any other document.
    ///
    /// Combines with [`Self::instructions`] when both are set — a shared file
    /// plus a line or two specific to this repository.
    #[serde(default)]
    pub instructions_file: Option<String>,
    /// Whether the reviewer answers with structured findings (the default) or
    /// free-form Markdown in the project's own style.
    #[serde(default)]
    pub output: OutputStyle,
    /// The house style for [`OutputStyle::Markdown`]: sections, tone, length,
    /// anything the review should look like. Ignored in findings mode.
    #[serde(default)]
    pub output_instructions: Option<String>,
    /// A file holding the house style, relative to the repository root. Same
    /// idea as [`Self::instructions_file`], and combines with
    /// [`Self::output_instructions`] the same way.
    #[serde(default)]
    pub output_instructions_file: Option<String>,
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

/// Enough tool calls to list a directory, search for a symbol, and read the
/// handful of files a change actually touches — not enough to walk the
/// repository. A reviewer that hits this still answers, on what it has.
fn default_context_calls() -> usize {
    24
}

/// Roughly a quarter of a million characters of source: generous for one
/// change, and a hard stop on a model that decides to read everything.
fn default_context_bytes() -> usize {
    200_000
}

/// How much diff a pull request description is written from.
///
/// Larger than a review's default: a description that misses half the branch
/// is wrong in a way nobody notices, where a review that misses half at least
/// reports fewer findings.
pub const MAX_PR_DIFF_BYTES: usize = 60_000;

/// Cap on an instructions file. Guidance shares the prompt with the diff, so
/// a runaway document would crowd out the code under review.
pub const MAX_INSTRUCTIONS_BYTES: usize = 32_000;

/// Reads an instructions file, confined to the repository.
///
/// The path comes from a committed config, which may have arrived with a
/// cloned repository, and its contents are sent to an AI provider. So the
/// resolved path must stay inside the worktree: absolute paths and `..`
/// escapes are refused rather than quietly read.
fn read_instructions_file(repo_root: &Path, rel: &str) -> std::result::Result<String, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!(
            "{rel}: must be relative to the repository root, not an absolute path"
        ));
    }
    let joined = repo_root.join(rel_path);
    let canonical = joined
        .canonicalize()
        .map_err(|_| format!("{rel}: no such file (relative to the repository root)"))?;
    let root = repo_root
        .canonicalize()
        .map_err(|e| format!("cannot resolve the repository root: {e}"))?;
    if !canonical.starts_with(&root) {
        return Err(format!("{rel}: resolves outside the repository"));
    }

    let text = std::fs::read_to_string(&canonical).map_err(|e| format!("{rel}: {e}"))?;
    if text.trim().is_empty() {
        return Err(format!("{rel}: is empty"));
    }
    if text.len() > MAX_INSTRUCTIONS_BYTES {
        let mut end = MAX_INSTRUCTIONS_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return Ok(format!("{}\n\n[instructions truncated]", &text[..end]));
    }
    Ok(text)
}

/// Joins inline text with a file's contents, either of which may be absent.
fn combine(inline: Option<&String>, from_file: Option<String>) -> Option<String> {
    let inline = inline.map(|s| s.trim()).filter(|s| !s.is_empty());
    match (inline, from_file) {
        (Some(a), Some(b)) => Some(format!("{a}\n\n{}", b.trim())),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.trim().to_string()),
        (None, None) => None,
    }
}

impl ReviewConfig {
    /// Resolves `*_file` paths into a config whose instruction fields hold the
    /// final text, so the providers never touch the filesystem.
    ///
    /// A missing or unreadable file is an error rather than a silent skip:
    /// reviewing without the rules you asked for produces a review that looks
    /// authoritative and is not the one you configured.
    pub fn resolve_files(&self, repo_root: &Path) -> std::result::Result<Self, String> {
        let mut resolved = self.clone();

        let review_file = match &self.instructions_file {
            Some(p) => Some(read_instructions_file(repo_root, p)?),
            None => None,
        };
        resolved.instructions = combine(self.instructions.as_ref(), review_file);
        resolved.instructions_file = None;

        // Only relevant in Markdown mode, but resolving unconditionally keeps
        // a broken path from hiding until someone switches output style.
        let style_file = match &self.output_instructions_file {
            Some(p) => Some(read_instructions_file(repo_root, p)?),
            None => None,
        };
        resolved.output_instructions =
            combine(self.output_instructions.as_ref(), style_file);
        resolved.output_instructions_file = None;

        Ok(resolved)
    }

    /// Whether a push should be reviewed.
    pub fn runs_on_push(&self) -> bool {
        self.on_push.unwrap_or(self.run)
    }

    /// Whether creating a pull request should be reviewed.
    pub fn runs_on_pull_request(&self) -> bool {
        self.on_pull_request.unwrap_or(self.run)
    }

    /// Whether the reviewer is enabled for any trigger at all. Used to decide
    /// whether to mention it in the UI.
    pub fn runs_at_all(&self) -> bool {
        self.runs_on_push() || self.runs_on_pull_request()
    }

    /// Budgets for a context-reading review, from the configured caps.
    pub fn limits(&self) -> crate::agent::Limits {
        let defaults = crate::agent::Limits::default();
        crate::agent::Limits {
            max_tool_calls: self.max_context_calls,
            max_read_bytes: self.max_context_bytes,
            // One turn per two calls covers a model that batches its reads,
            // bounded so a loop cannot outlive the call budget.
            max_turns: (self.max_context_calls / 2).clamp(4, 16),
            ..defaults
        }
    }
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            run: false,
            on_push: None,
            on_pull_request: None,
            block_on_failure: true,
            fail_on: default_fail_on(),
            max_diff_bytes: default_max_diff(),
            repo_context: true,
            max_context_calls: default_context_calls(),
            max_context_bytes: default_context_bytes(),
            verify_findings: true,
            provider: None,
            model: None,
            instructions: None,
            instructions_file: None,
            output: OutputStyle::Findings,
            output_instructions: None,
            output_instructions_file: None,
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
    "detail": "why it is wrong, the input or state that triggers it, and the fix",
    "evidence": "the exact line of code, copied verbatim from the file, that shows it"}
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
- Every finding needs "evidence": one line copied **verbatim** from the file you are accusing. It is checked against the file, and a finding whose evidence is not there is dropped. Copy, do not paraphrase or reconstruct from memory.
- Before reporting that something is missing, unchecked, or uninitialised, go and look for the thing that would provide it: a `#[serde(default)]`, a `Default` impl, a guard earlier in the function, a check in the only caller, a fallback in the match arm above. Read it. A diff hunk cannot tell you what the rest of the file already guarantees, and "X may not be initialised" is not a finding when three lines away X has a default.
- If you cannot open a file you would need to judge a finding, say so in "reasoning" and lower the severity. An unverified guess reported as high is worse than no review.
- Do not restate what the diff does, praise it, or suggest unrelated refactors.
- An empty "findings" array is the correct answer for a clean change. Do not invent findings to appear thorough."#;

/// The line a Markdown-mode reviewer must lead with when the gate is armed,
/// so a free-form review can still hold a push. Stripped before rendering.
pub const VERDICT_PREFIX: &str = "VERDICT:";

/// System prompt for [`OutputStyle::Markdown`]: the review criteria stay the
/// same, only the output contract changes to the project's own style.
///
/// `require_verdict` adds the machine-readable first line the gate needs.
/// Without it the review is advisory and nothing is parsed out of the body,
/// which is the point of this mode — the user owns the format.
pub fn markdown_system_prompt(style: Option<&str>, require_verdict: bool) -> String {
    let mut p = String::from(
        "You are reviewing a git diff before it is pushed or opened as a pull request. \
         Report defects in the changed code.\n\n\
         What to look for:\n\
         - Incorrectness and unsafety first: wrong results, crashes, data loss, races, \
         resource leaks, injection, auth or secret exposure, a broken API contract.\n\
         - Then real problems that are not correctness failures: missing error handling \
         on a path that can fail, a missing test for new branching logic, a performance \
         cliff, a misleading name or comment that will cause a future bug.\n\
         - Style and naming last, and only briefly.\n\n\
         Rules:\n\
         - Judge only the changed lines and code they directly affect. Do not report \
         pre-existing issues in untouched code.\n\
         - Give a concrete failing case for each problem: the input, state, or sequence \
         that produces the bad outcome. If you cannot name one, say so plainly instead \
         of asserting it.\n\
         - Cite locations as file and line.\n\
         - Do not restate what the diff does, praise it, or suggest unrelated refactors.\n\
         - Saying the change looks correct is a valid review. Do not invent problems to \
         appear thorough.\n\n",
    );

    if require_verdict {
        p.push_str(&format!(
            "Your response MUST begin with exactly one of these two lines, on its own \
             line, before anything else:\n\
             {VERDICT_PREFIX} block\n\
             {VERDICT_PREFIX} pass\n\
             Use \"block\" only when you found something that should be fixed before \
             this code is published; \"pass\" otherwise. The line is read by the tool \
             and removed before your review is shown.\n\n"
        ));
    }

    p.push_str("Write the rest of your response as GitHub-flavoured Markdown.");

    match style.map(str::trim).filter(|s| !s.is_empty()) {
        Some(style) => {
            p.push_str(&format!(
                " Follow this house style exactly — it overrides any formatting \
                 preference of your own:\n\n{style}"
            ));
        }
        None => {
            p.push_str(
                " Lead with a one-line verdict, then the problems worth acting on, \
                 worst first. Keep it short enough to read in full.",
            );
        }
    }
    p
}

/// Appended to whichever system prompt the output style selects, when the
/// reviewer has read-only repository access.
///
/// The emphasis on *why* to read is deliberate. A model handed tools will
/// otherwise either ignore them or spend its whole budget listing files; what
/// makes a review better is opening the definition of the thing the diff
/// calls and checking the assumption the diff is making about it.
pub const CONTEXT_PROMPT: &str = r#"You can read this repository while you review. Use it.

Tools: list_files, read_file, search. They see every file git tracks, and nothing else.

A diff shows changed lines, not whether they are correct. Open what you need to decide:
- The definition of anything the change calls, to check arguments, error cases, and contracts.
- Other callers of anything the change alters, to see what a changed signature or behaviour breaks.
- The tests covering the changed code, to see whether this change is tested at all.
- The rest of the file each hunk sits in, when the surrounding code decides whether the hunk is right.

Read deliberately, not exhaustively: a handful of targeted reads beats crawling the tree, and your budget is finite. When it runs out you must answer with what you have.

Then judge the change against what you read, not against what the diff alone suggests. If a file you needed was unreadable, say so in your answer rather than guessing."#;

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Checks every finding against the code and drops the ones that do not
/// survive, recording why in the reading list.
///
/// Two gates, cheapest first. The citation check is mechanical: a quoted
/// line is either in the file or it is not. What survives that goes back to
/// the model with the repository still open, asked the one question the
/// first pass never asks itself — *is this actually true of this code?*
///
/// A verification that cannot run leaves the findings alone. A gate that
/// silently discarded findings because a second request failed would be
/// worse than one that reports too many.
pub fn verify(
    provider: &dyn crate::agent::Provider,
    workspace: &mut crate::agent::Workspace,
    outcome: &mut ReviewOutcome,
    config: &ReviewConfig,
    on_event: &mut dyn FnMut(crate::agent::Event),
) {
    if outcome.findings.is_empty() {
        return;
    }
    let before = outcome.findings.len();
    let read = |path: &str| workspace.read_tracked(path);

    // 1. Mechanical: does the quoted line exist in the file it names?
    let mut kept: Vec<Finding> = Vec::new();
    for mut finding in std::mem::take(&mut outcome.findings) {
        match check_citation(&finding, &read) {
            Ok(line) => {
                finding.line = line;
                kept.push(finding);
            }
            Err(why) => outcome
                .context_log
                .push(format!("! dropped \"{}\" — {why}", finding.title)),
        }
    }

    // 2. The model, with the whole repository, judging its own findings.
    if !kept.is_empty() {
        // The budget is per finding, not per review. Verification checks each
        // finding independently — a couple of reads apiece — so a fixed pool
        // is spent on the first few and the rest are never examined. They are
        // then kept, which is right, and indistinguishable from findings that
        // survived scrutiny, which is not.
        let limits = crate::agent::Limits {
            max_tool_calls: (CALLS_PER_FINDING * kept.len())
                .clamp(config.max_context_calls, MAX_VERIFY_CALLS),
            max_turns: (TURNS_PER_FINDING * kept.len()).clamp(6, MAX_VERIFY_TURNS),
            ..config.limits()
        };
        match crate::agent::run(
            provider,
            workspace,
            VERIFY_SYSTEM_PROMPT,
            &verify_prompt(&kept),
            limits,
            on_event,
        ) {
            Ok(run) => {
                outcome.context_log.extend(run.log);
                let verdicts = parse_verdicts(&run.text);
                if verdicts.is_empty() {
                    outcome
                        .context_log
                        .push("! verification returned nothing usable; findings kept".into());
                } else {
                    let mut survivors = Vec::new();
                    let mut unjudged = 0;
                    for (i, mut finding) in kept.into_iter().enumerate() {
                        match verdicts.iter().find(|(index, ..)| *index == i) {
                            Some((_, false, why)) => outcome.context_log.push(format!(
                                "! dropped \"{}\" — {why}",
                                finding.title
                            )),
                            Some((_, true, _)) => {
                                finding.verified = true;
                                survivors.push(finding);
                            }
                            // Unjudged findings are kept: silence is not a
                            // verdict. But they are reported as unchecked
                            // rather than passed off as having been examined.
                            None => {
                                unjudged += 1;
                                outcome.context_log.push(format!(
                                    "? not verified \"{}\" — the verifier did not \
                                     reach it; kept unchecked",
                                    finding.title
                                ));
                                survivors.push(finding);
                            }
                        }
                    }
                    if unjudged > 0 {
                        outcome.context_log.push(format!(
                            "? {unjudged} finding(s) were kept without being checked"
                        ));
                    }
                    kept = survivors;
                }
            }
            Err(e) => outcome
                .context_log
                .push(format!("! verification could not run ({e}); findings kept")),
        }
    }

    let dropped = before - kept.len();
    if dropped > 0 {
        outcome
            .context_log
            .push(format!("· {dropped} of {before} findings did not survive verification"));
    }
    outcome.findings = kept;
}

/// How a finding fared when it was checked against the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The evidence is in the file and the finding survived a second look.
    Confirmed,
    /// Dropped, with the reason.
    Dropped(String),
}

/// Checks a finding's citation against the file it accuses.
///
/// Purely mechanical, and it catches the most common way a review wastes
/// somebody's afternoon: a confident finding about code that does not say
/// what the reviewer thinks it says. A quote either appears in the file or
/// it does not.
fn check_citation(
    finding: &Finding,
    read: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<u32>, String> {
    if finding.file.trim().is_empty() {
        // A finding about the change as a whole cites nothing; there is
        // nothing to check.
        return Ok(finding.line);
    }
    let Some(content) = read(&finding.file) else {
        return Err(format!("{} is not a file in this repository", finding.file));
    };

    // A line number past the end is a citation of nothing.
    let total = content.lines().count() as u32;
    let line = finding.line.filter(|l| *l >= 1 && *l <= total);

    if finding.evidence.trim().is_empty() {
        return Ok(line);
    }
    // Whitespace is not evidence: models re-indent when they quote.
    let squash = |text: &str| {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let haystack = squash(&content);
    let needle = squash(&finding.evidence);
    if needle.is_empty() || haystack.contains(&needle) {
        // Point the finding at where the evidence actually is, when the
        // reported line was wrong.
        let found = content
            .lines()
            .position(|l| squash(l).contains(&needle) || needle.contains(&squash(l)) && !l.trim().is_empty())
            .map(|i| i as u32 + 1);
        return Ok(line.or(found));
    }
    Err(format!(
        "the quoted line is not in {}: {}",
        finding.file,
        excerpt(&finding.evidence)
    ))
}

/// Which model reviews, and where to reach it.
///
/// Lives here rather than in the app, because the app is not the only thing
/// that reviews: `devdock push` gates on the same config and has to run the
/// same review, and it has no egui context to borrow the logic from.
#[derive(Debug, Clone)]
pub struct Reviewer {
    /// "claude" or "ollama".
    pub provider: String,
    pub model: String,
    /// Base URL for Ollama; ignored for Claude.
    pub ollama_url: String,
}

impl Reviewer {
    /// The reviewer a config asks for, if it names one.
    ///
    /// `fallback` is what the caller would use otherwise — the app's own
    /// model picker, or nothing at all on the command line.
    pub fn resolve(
        config: &ReviewConfig,
        fallback: Option<(String, String)>,
        ollama_url: &str,
    ) -> Option<Self> {
        let (provider, model) = match (&config.provider, &config.model) {
            (Some(p), Some(m)) => (p.clone(), m.clone()),
            _ => {
                let (p, m) = fallback?;
                (
                    config.provider.clone().unwrap_or(p),
                    config.model.clone().unwrap_or(m),
                )
            }
        };
        Some(Self { provider, model, ollama_url: ollama_url.to_string() })
    }

    fn agent(&self) -> std::result::Result<Box<dyn crate::agent::Provider>, String> {
        if self.provider == "claude" {
            return crate::claude::Client::from_store(self.model.clone())
                .map(|c| Box::new(c) as Box<dyn crate::agent::Provider>)
                .ok_or_else(|| "Claude is not signed in.".to_string());
        }
        if self.model.trim().is_empty() {
            return Err("No Ollama model selected.".into());
        }
        Ok(Box::new(crate::ollama::Client::new(&self.ollama_url).agent(self.model.clone())))
    }

    /// The diff-only review: one request, no tools. Used when `repo_context`
    /// is off, and as the fallback for a model that cannot call tools.
    fn single_shot(
        &self,
        diff: &str,
        config: &ReviewConfig,
    ) -> std::result::Result<ReviewOutcome, String> {
        if self.provider == "claude" {
            let client = crate::claude::Client::from_store(self.model.clone())
                .ok_or("Claude is not signed in.")?;
            return client.review(diff, config).map_err(|e| e.to_string());
        }
        crate::ollama::Client::new(&self.ollama_url)
            .review(&self.model, diff, config)
            .map_err(|e| e.to_string())
    }
}

/// Reviews `diff` the way the configuration asks for, with the repository
/// open to the model unless `repo_context` is off.
///
/// One path, used by the app's gate, its Checks tab, and `devdock push`, so
/// that all three agree about what a review is.
pub fn review_diff(
    repo: &crate::git::Repo,
    reviewer: &Reviewer,
    diff: &str,
    config: &ReviewConfig,
    on_event: &mut dyn FnMut(crate::agent::Event),
) -> std::result::Result<ReviewOutcome, String> {
    let coverage = coverage_note(diff, config.max_diff_bytes);
    let mut outcome = if !config.repo_context {
        reviewer.single_shot(diff, config)?
    } else {
        match with_repo_context(repo, reviewer, diff, config, on_event) {
            Err(e) if lacks_tool_support(&e) => {
                // The model cannot call tools, so it cannot read the
                // repository. Review the diff alone rather than failing: a
                // gate that errors out gets switched off.
                let mut outcome = reviewer.single_shot(diff, config)?;
                outcome.context_log.push(format!("! {e}"));
                outcome
                    .context_log
                    .push("! reviewed the diff alone, without repository context".into());
                outcome
            }
            other => other?,
        }
    };
    if let Some(note) = coverage {
        outcome.context_log.push(note);
    }
    Ok(outcome)
}

fn with_repo_context(
    repo: &crate::git::Repo,
    reviewer: &Reviewer,
    diff: &str,
    config: &ReviewConfig,
    on_event: &mut dyn FnMut(crate::agent::Event),
) -> std::result::Result<ReviewOutcome, String> {
    let provider = reviewer.agent()?;
    let tracked = repo.tracked_files().map_err(|e| e.to_string())?;
    let mut workspace =
        crate::agent::Workspace::new(repo.path(), tracked, crate::agent::Access::ReadOnly)?;
    run_with_context(provider.as_ref(), &mut workspace, diff, config, on_event)
}

/// Whether a failure means "this model cannot call tools", which is worth
/// falling back for, as opposed to a real error worth reporting.
pub fn lacks_tool_support(error: &str) -> bool {
    let e = error.to_lowercase();
    // Only these mean "this model cannot do tools at all". Anything else — a
    // timeout, a 500, a refused connection — must propagate: silently
    // downgrading to a diff-only review on a transient failure would hide
    // that the reviewer never got its context.
    [
        "cannot call tools",
        "does not support tools",
        "tools are not supported",
        "tool use is not supported",
        "does not support tool",
    ]
    .iter()
    .any(|phrase| e.contains(phrase))
}

/// Tool calls the verifier is given per finding it has to check.
///
/// Two reads and a search is a realistic cost for deciding whether one
/// finding is real: the file it names, whatever calls it, and a look for the
/// definition of whatever it depends on.
const CALLS_PER_FINDING: usize = 8;

/// Turns per finding.
///
/// Turns, not calls, are what runs out: a model batches its reads, so it
/// spends a turn per question it wants answered rather than per file. Four is
/// what it took to check a finding that needed a definition looked up and its
/// caller read — with three, a four-finding review hit the ceiling with two
/// findings still unexamined.
const TURNS_PER_FINDING: usize = 4;

/// Ceilings, so a review that somehow produced fifty findings cannot spend an
/// unbounded number of requests checking them.
const MAX_VERIFY_CALLS: usize = 120;
const MAX_VERIFY_TURNS: usize = 40;

const VERIFY_SYSTEM_PROMPT: &str = r#"You are checking a code review before it is shown to the developer. For each finding, decide whether it is a real defect in this code.

You have the same tools as the reviewer: read the file, read what it calls, read its callers. Use them. The reviewer worked partly from a diff; you have the whole repository.

Drop a finding when:
- The thing it says is missing is already provided somewhere it did not look — a serde default, a Default impl, an earlier guard, a check in the only caller, the match arm above the cited line.
- It describes the deliberate, documented design of the code, rather than a mistake. Read the doc comment before deciding.
- Its failing case cannot actually happen: the input it needs is rejected earlier, the state it needs is unreachable.
- It is about code the diff did not change.
- It restates what the code does without saying what goes wrong.

Keep a finding when the defect is real, even if it is small, and even if fixing it is easy.

Answer with JSON only:
{"verdicts": [{"index": 0, "keep": true, "why": "confirmed: the caller does not check this"},
              {"index": 1, "keep": false, "why": "repo_context has #[serde(default = "default_true")] on line 231"}]}

"why" is one sentence, and for a dropped finding it must name the specific code that makes it wrong. Judge every finding you were given, by index."#;

/// One finding as the verifier sees it.
fn verify_prompt(findings: &[Finding]) -> String {
    let mut prompt = String::from(
        "Check these findings against the code. Read whatever you need.\n",
    );
    for (i, finding) in findings.iter().enumerate() {
        prompt.push_str(&format!(
            "\n[{i}] {} ({}){}\n{}\n",
            finding.title,
            finding.severity.label(),
            match (finding.file.is_empty(), finding.line) {
                (true, _) => String::new(),
                (false, Some(line)) => format!(" — {}:{line}", finding.file),
                (false, None) => format!(" — {}", finding.file),
            },
            finding.detail
        ));
        if !finding.evidence.trim().is_empty() {
            prompt.push_str(&format!("quoted: {}\n", finding.evidence.trim()));
        }
    }
    prompt
}

/// Parses the verifier's answer into `(index, keep, why)`.
fn parse_verdicts(text: &str) -> Vec<(usize, bool, String)> {
    let Some(start) = text.find('{') else { return Vec::new() };
    let Some(end) = text.rfind('}') else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[start..=end]) else {
        return Vec::new();
    };
    let Some(items) = value.get("verdicts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some((
                item.get("index")?.as_u64()? as usize,
                item.get("keep").and_then(|k| k.as_bool()).unwrap_or(true),
                item.get("why").and_then(|w| w.as_str()).unwrap_or("").to_string(),
            ))
        })
        .collect()
}

/// Runs a review whose model can read the repository, and returns the
/// parsed outcome with the reviewer's reading list attached.
///
/// This is the same review contract as the single-shot path in
/// [`crate::claude`] and [`crate::ollama`] — same prompts, same parsing,
/// same gate — with tools added. Callers fall back to the single-shot path
/// when `repo_context` is off or the model cannot call tools.
pub fn run_with_context(
    provider: &dyn crate::agent::Provider,
    workspace: &mut crate::agent::Workspace,
    diff: &str,
    config: &ReviewConfig,
    on_event: &mut dyn FnMut(crate::agent::Event),
) -> std::result::Result<ReviewOutcome, String> {
    let base = match config.output {
        OutputStyle::Findings => SYSTEM_PROMPT.to_string(),
        OutputStyle::Markdown => markdown_system_prompt(
            config.output_instructions.as_deref(),
            config.block_on_failure,
        ),
    };
    let system = format!("{base}\n\n{CONTEXT_PROMPT}");
    let prompt = user_prompt(diff, config.instructions.as_deref(), config.max_diff_bytes);

    let run = crate::agent::run(
        provider,
        workspace,
        &system,
        &prompt,
        config.limits(),
        on_event,
    )?;

    let mut outcome = match config.output {
        OutputStyle::Findings => {
            if !parsed_cleanly(&run.text) {
                return Err(format!(
                    "The reviewer did not return a usable review: {}",
                    excerpt(&run.text)
                ));
            }
            parse(&run.text)
        }
        OutputStyle::Markdown => {
            if run.text.trim().is_empty() {
                return Err("The reviewer returned nothing.".into());
            }
            parse_markdown(&run.text)
        }
    };
    outcome.context_log = run.log;

    // Findings are checked against the code before anyone is asked to act
    // on them. Markdown mode has no findings to check.
    if config.output == OutputStyle::Findings && config.verify_findings {
        verify(provider, workspace, &mut outcome, config, on_event);
    }
    if run.truncated {
        outcome
            .context_log
            .push("! budget spent; the review was finished on partial context".into());
    }
    Ok(outcome)
}

/// Extracts a Markdown-mode review: strips an outer code fence and the
/// verdict line, keeping the body for rendering.
pub fn parse_markdown(text: &str) -> ReviewOutcome {
    // Models sometimes wrap the whole answer in a ```markdown fence.
    let body = strip_outer_fence(text.trim());

    let mut verdict_blocks = false;
    let mut summary = String::new();
    let mut lines = body.lines();
    let mut rest_start = 0usize;

    // The verdict line is only honoured at the very top, so the word
    // appearing later in prose cannot flip the gate.
    for line in lines.by_ref() {
        let trimmed = line.trim();
        rest_start += line.len() + 1;
        if trimmed.is_empty() {
            continue;
        }
        if let Some(v) = trimmed.strip_prefix(VERDICT_PREFIX) {
            verdict_blocks = v.trim().eq_ignore_ascii_case("block");
            break;
        }
        // No verdict line: nothing to strip.
        rest_start = 0;
        break;
    }

    let markdown = if rest_start > 0 && rest_start <= body.len() {
        body[rest_start..].trim_start().to_string()
    } else {
        body.to_string()
    };

    // First non-heading, non-empty line doubles as the one-line summary in
    // compact places like the Checks tab header.
    for line in markdown.lines() {
        let t = line.trim().trim_start_matches('#').trim();
        if !t.is_empty() {
            summary = t.to_string();
            break;
        }
    }

    ReviewOutcome {
        summary,
        reasoning: String::new(),
        findings: Vec::new(),
        markdown: Some(markdown),
        verdict_blocks,
        context_log: Vec::new(),
    }
}

/// Removes a fence wrapping the entire text, leaving inner fences alone.
fn strip_outer_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else { return text };
    let Some((_info, body)) = rest.split_once('\n') else { return text };
    match body.trim_end().strip_suffix("```") {
        Some(inner) => inner.trim_end(),
        None => text,
    }
}

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

/// How much of the diff the reviewer was actually shown, when it was not
/// all of it.
///
/// A review of 8% of a change reads exactly like a review of all of it —
/// same confident tone, same clean bill of health for everything it never
/// saw. The only defence is to say so, next to the findings.
pub fn coverage_note(diff: &str, max_diff_bytes: usize) -> Option<String> {
    if diff.len() <= max_diff_bytes {
        return None;
    }
    let percent = (max_diff_bytes as f64 / diff.len() as f64 * 100.0).round() as u32;
    Some(format!(
        "! the diff is {} bytes and only the first {max_diff_bytes} were reviewed \
         ({percent}%) — raise max_diff_bytes under [review], or review a smaller change",
        diff.len()
    ))
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
    #[serde(default)]
    evidence: String,
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
            evidence: f.evidence.trim().to_string(),
            // Nothing has checked it yet; `verify` is what sets this.
            verified: false,
        })
        .collect();

    ReviewOutcome {
        summary: raw.summary.trim().to_string(),
        reasoning: raw.reasoning.trim().to_string(),
        findings,
        markdown: None,
        verdict_blocks: false,
        context_log: Vec::new(),
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
    fn markdown_mode_keeps_the_body_verbatim() {
        let out = parse_markdown("## Verdict\n\nLooks wrong in `foo()`.\n");
        assert_eq!(out.markdown.as_deref(), Some("## Verdict\n\nLooks wrong in `foo()`."));
        assert!(out.findings.is_empty(), "markdown mode has no structured findings");
        // The first meaningful line doubles as the compact summary.
        assert_eq!(out.summary, "Verdict");
    }

    #[test]
    fn verdict_line_drives_the_gate_and_is_stripped() {
        let block = parse_markdown("VERDICT: block\n\n## Problems\n\nRace in `sync`.\n");
        assert!(block.verdict_blocks);
        let md = block.markdown.unwrap();
        assert!(!md.contains("VERDICT"), "verdict line must not be rendered: {md:?}");
        assert!(md.starts_with("## Problems"));

        let pass = parse_markdown("VERDICT: pass\n\nLooks fine.\n");
        assert!(!pass.verdict_blocks);
        assert_eq!(pass.markdown.as_deref(), Some("Looks fine."));
    }

    /// The word appearing later in prose must not flip the gate.
    #[test]
    fn verdict_is_only_honoured_at_the_top() {
        let out = parse_markdown("## Notes\n\nI would VERDICT: block this normally.\n");
        assert!(!out.verdict_blocks);
        assert!(out.markdown.unwrap().contains("VERDICT: block"), "prose kept verbatim");
    }

    #[test]
    fn an_outer_fence_is_stripped_but_inner_ones_survive() {
        let out = parse_markdown("```markdown\n## R\n\n```rust\nlet a = 1;\n```\n");
        let md = out.markdown.unwrap();
        assert!(md.starts_with("## R"), "outer fence not stripped: {md:?}");
        assert!(md.contains("```rust"), "inner fence must survive: {md:?}");
    }

    #[test]
    fn should_block_uses_the_right_rule_per_output_style() {
        let mut cfg = ReviewConfig { block_on_failure: true, ..Default::default() };

        // Findings mode: the fail_on threshold decides.
        let findings = parse(r#"{"findings":[{"severity":"medium","title":"t","detail":"d"}]}"#);
        cfg.output = OutputStyle::Findings;
        cfg.fail_on = Severity::High;
        assert!(!findings.should_block(&cfg));
        cfg.fail_on = Severity::Medium;
        assert!(findings.should_block(&cfg));

        // Markdown mode: the verdict line decides, and fail_on is irrelevant.
        cfg.output = OutputStyle::Markdown;
        cfg.fail_on = Severity::High;
        assert!(parse_markdown("VERDICT: block\n\nbad").should_block(&cfg));
        assert!(!parse_markdown("VERDICT: pass\n\nfine").should_block(&cfg));
        // No verdict line at all: advisory, never blocks.
        assert!(!parse_markdown("just prose").should_block(&cfg));

        // block_on_failure = false never blocks in either mode.
        cfg.block_on_failure = false;
        assert!(!parse_markdown("VERDICT: block\n\nbad").should_block(&cfg));
        cfg.output = OutputStyle::Findings;
        cfg.fail_on = Severity::Low;
        assert!(!findings.should_block(&cfg));
    }

    #[test]
    fn markdown_prompt_carries_the_house_style_and_verdict_contract() {
        let styled = markdown_system_prompt(Some("Use ## Verdict then ## Nits."), true);
        assert!(styled.contains("## Verdict then ## Nits."), "house style must be included");
        assert!(styled.contains(VERDICT_PREFIX), "gate needs the verdict contract");

        // Advisory mode asks for no verdict line.
        let advisory = markdown_system_prompt(None, false);
        assert!(!advisory.contains(VERDICT_PREFIX));
        // Still Markdown, still the same review criteria.
        assert!(advisory.contains("Markdown"));
        assert!(advisory.contains("data loss"));
    }

    #[test]
    fn run_covers_both_triggers() {
        let cfg = ReviewConfig { run: true, ..Default::default() };
        assert!(cfg.runs_on_push());
        assert!(cfg.runs_on_pull_request());
        assert!(cfg.runs_at_all());
    }

    #[test]
    fn off_by_default_for_both_triggers() {
        let cfg = ReviewConfig::default();
        assert!(!cfg.runs_on_push());
        assert!(!cfg.runs_on_pull_request());
        assert!(!cfg.runs_at_all());
    }

    /// Either trigger can be enabled on its own, without `run`.
    #[test]
    fn a_single_trigger_can_be_enabled_alone() {
        let push_only = ReviewConfig { on_push: Some(true), ..Default::default() };
        assert!(push_only.runs_on_push());
        assert!(!push_only.runs_on_pull_request());
        assert!(push_only.runs_at_all());

        let pr_only = ReviewConfig { on_pull_request: Some(true), ..Default::default() };
        assert!(!pr_only.runs_on_push());
        assert!(pr_only.runs_on_pull_request());
    }

    /// An explicit per-trigger `false` overrides `run = true`, so you can
    /// review PRs but not every push.
    #[test]
    fn a_trigger_can_opt_out_of_run() {
        let cfg = ReviewConfig { run: true, on_push: Some(false), ..Default::default() };
        assert!(!cfg.runs_on_push(), "explicit false must win over run");
        assert!(cfg.runs_on_pull_request());
        assert!(cfg.runs_at_all());

        // Opting out of both is explicit and allowed.
        let none = ReviewConfig {
            run: true,
            on_push: Some(false),
            on_pull_request: Some(false),
            ..Default::default()
        };
        assert!(!none.runs_at_all());
    }

    #[test]
    fn triggers_parse_from_toml_including_the_on_pr_spelling() {
        let cfg: ReviewConfig =
            toml::from_str("on_push = true\non_pr = false\n").expect("should parse");
        assert!(cfg.runs_on_push());
        assert!(!cfg.runs_on_pull_request(), "on_pr should alias on_pull_request");

        // The long spelling works too.
        let long: ReviewConfig = toml::from_str("on_pull_request = true\n").unwrap();
        assert!(long.runs_on_pull_request());
        assert!(!long.runs_on_push(), "unset trigger falls back to run = false");
    }

    #[test]
    fn output_style_defaults_to_findings() {
        assert_eq!(ReviewConfig::default().output, OutputStyle::Findings);
    }

    #[test]
    fn default_config_is_off_and_blocks_only_on_high() {
        let c = ReviewConfig::default();
        assert!(!c.run, "review must be opt-in");
        assert!(c.block_on_failure);
        assert_eq!(c.fail_on, Severity::High);
    }

    #[test]
    fn coverage_is_reported_only_when_the_diff_was_cut() {
        assert!(coverage_note("small diff", 24_000).is_none());

        let big = "x".repeat(300_000);
        let note = coverage_note(&big, 24_000).expect("a cut diff must be reported");
        assert!(note.contains("300000"), "{note}");
        assert!(note.contains("24000"), "{note}");
        // The percentage is what makes it land: "8%" is a different claim
        // from "the diff was truncated".
        assert!(note.contains("8%"), "{note}");
        assert!(note.contains("max_diff_bytes"), "{note}");
    }
}
