//! Ollama client: generates commit messages from diffs via a local server.
//!
//! Blocking HTTP; call from a worker thread in GUI code.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default local Ollama endpoint.
pub const DEFAULT_URL: &str = "http://localhost:11434";

const MAX_DIFF_CHARS: usize = 24_000;
const SYSTEM_PROMPT: &str = "You are an expert software engineer writing git commit messages. \
Given a diff, produce a concise conventional-commit style summary line (max 72 chars, imperative mood, \
e.g. 'feat: add user login') and a short description body explaining what changed and why. \
Write the description in GitHub-flavored Markdown (bullet lists, `code` spans, ### headings \
where they help readability). \
Respond only with JSON: {\"summary\": \"...\", \"description\": \"...\"}";

/// Errors from Ollama requests. Messages are user-presentable.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct OllamaError(pub String);

pub type Result<T> = std::result::Result<T, OllamaError>;

/// A model available on the Ollama server.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Model {
    pub name: String,
    #[serde(default)]
    pub size: u64,
}

/// AI-generated commit message.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitSuggestion {
    /// Single-line summary, at most 72 characters.
    pub summary: String,
    /// Longer body; may be empty.
    pub description: String,
}

/// Client for one Ollama server (blocking).
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
}

impl Client {
    /// Creates a client for `base_url`, e.g. [`DEFAULT_URL`].
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self { base_url }
    }

    /// Lists models installed on the server.
    pub fn models(&self) -> Result<Vec<Model>> {
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Model>,
        }
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()
            .get(&format!("{}/api/tags", self.base_url))
            .call()
            .map_err(|e| OllamaError(format!("Cannot reach Ollama at {}: {e}", self.base_url)))?;
        let tags: Tags =
            resp.into_json().map_err(|e| OllamaError(format!("Bad response from Ollama: {e}")))?;
        Ok(tags.models)
    }

    /// Generates a commit message for `diff` using `model`.
    ///
    /// `extra_instructions` is appended to the system prompt, letting users
    /// customize style per repository (e.g. ticket prefixes, language).
    pub fn commit_message(
        &self,
        model: &str,
        diff: &str,
        extra_instructions: Option<&str>,
    ) -> Result<CommitSuggestion> {
        if diff.trim().is_empty() {
            return Err(OllamaError(
                "No changes to describe. Stage or modify some files first.".into(),
            ));
        }
        let system = match extra_instructions.filter(|s| !s.trim().is_empty()) {
            Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
            None => SYSTEM_PROMPT.to_string(),
        };
        let prompt = format!(
            "Write a commit message for this diff:\n\n```diff\n{}\n```",
            truncate_utf8(diff, MAX_DIFF_CHARS)
        );
        let payload = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "system": system,
            "stream": false,
            "format": {
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["summary", "description"]
            },
            "options": {"temperature": 0.2}
        });
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(300))
            .build()
            .post(&format!("{}/api/generate", self.base_url))
            .send_json(payload)
            .map_err(|e| OllamaError(format!("Ollama request failed: {e}")))?;
        let value: serde_json::Value =
            resp.into_json().map_err(|e| OllamaError(format!("Bad response from Ollama: {e}")))?;
        let text = value
            .get("response")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OllamaError("Ollama returned no response text".into()))?;
        Ok(parse_suggestion(text))
    }

    /// Asks the model to draft a `.git-manage-ci.toml` from a repo scan.
    /// Returns raw TOML text (a proposal for the user to review).
    pub fn generate_ci_config(&self, model: &str, repo_scan: &str) -> Result<String> {
        let prompt = format!(
            "Write .git-manage-ci.toml for this repository:\n\n{repo_scan}"
        );
        let payload = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "system": crate::local_ci::AI_CONFIG_SYSTEM_PROMPT,
            "stream": false,
            "options": {"temperature": 0.1}
        });
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(300))
            .build()
            .post(&format!("{}/api/generate", self.base_url))
            .send_json(payload)
            .map_err(|e| OllamaError(format!("Ollama request failed: {e}")))?;
        let value: serde_json::Value =
            resp.into_json().map_err(|e| OllamaError(format!("Bad response from Ollama: {e}")))?;
        let text = value
            .get("response")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OllamaError("Ollama returned no response text".into()))?;
        Ok(extract_merged_content(text))
    }

    /// Reviews an outgoing diff. See [`crate::review`] for the contract.
    pub fn review(
        &self,
        model: &str,
        diff: &str,
        config: &crate::review::ReviewConfig,
    ) -> Result<crate::review::ReviewOutcome> {
        use crate::review::{self, OutputStyle};

        let markdown_mode = config.output == OutputStyle::Markdown;
        let system = if markdown_mode {
            review::markdown_system_prompt(
                config.output_instructions.as_deref(),
                config.block_on_failure,
            )
        } else {
            review::SYSTEM_PROMPT.to_string()
        };
        let prompt =
            review::user_prompt(diff, config.instructions.as_deref(), config.max_diff_bytes);
        let payload = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "system": system,
            "stream": false,
            // Reviews should be reproducible run to run, and the schema is
            // fixed, so there is nothing for sampling variance to add.
            "options": {"temperature": 0.1}
        });
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(300))
            .build()
            .post(&format!("{}/api/generate", self.base_url))
            .send_json(payload)
            .map_err(|e| OllamaError(format!("Ollama request failed: {e}")))?;
        let value: serde_json::Value =
            resp.into_json().map_err(|e| OllamaError(format!("Bad response from Ollama: {e}")))?;
        let text = value
            .get("response")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OllamaError("Ollama returned no response text".into()))?;
        if markdown_mode {
            if text.trim().is_empty() {
                return Err(OllamaError("The reviewer returned nothing.".into()));
            }
            return Ok(review::parse_markdown(text));
        }
        if !review::parsed_cleanly(text) {
            return Err(OllamaError(format!(
                "The reviewer did not return a usable review: {}. Smaller local \
                 models often cannot hold the JSON format — try a larger model, \
                 set provider = \"claude\", or use output = \"markdown\" under \
                 [review], which has no format to parse.",
                review::excerpt(text)
            )));
        }
        Ok(review::parse(text))
    }

    /// Asks the model to merge a conflicted file from its three stages.
    /// Returns the full merged file content.
    pub fn resolve_conflict(
        &self,
        model: &str,
        path: &str,
        base: &str,
        ours: &str,
        theirs: &str,
        extra_instructions: Option<&str>,
    ) -> Result<String> {
        let prompt = merge_prompt(path, base, ours, theirs, MAX_MERGE_INPUT_CHARS)
            .map_err(OllamaError)?;
        let system = merge_system_prompt(extra_instructions);
        let payload = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "system": system,
            "stream": false,
            "options": {"temperature": 0.0}
        });
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(300))
            .build()
            .post(&format!("{}/api/generate", self.base_url))
            .send_json(payload)
            .map_err(|e| OllamaError(format!("Ollama request failed: {e}")))?;
        let value: serde_json::Value =
            resp.into_json().map_err(|e| OllamaError(format!("Bad response from Ollama: {e}")))?;
        let text = value
            .get("response")
            .and_then(|r| r.as_str())
            .ok_or_else(|| OllamaError("Ollama returned no response text".into()))?;
        Ok(extract_merged_content(text))
    }
}

/// The merge system prompt, with the user's custom instructions appended
/// when configured. The built-in prompt always applies; custom text extends
/// it rather than replacing it, so output-format rules stay intact.
pub fn merge_system_prompt(extra_instructions: Option<&str>) -> String {
    match extra_instructions.filter(|s| !s.trim().is_empty()) {
        Some(extra) => format!("{MERGE_SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => MERGE_SYSTEM_PROMPT.to_string(),
    }
}

const MERGE_SYSTEM_PROMPT: &str = "You are an expert software engineer resolving a git merge \
conflict. You are given the common ancestor (BASE), the current branch's version (OURS), and \
the incoming version (THEIRS) of one file. Produce the correctly merged file: keep the intent \
of BOTH sides' changes wherever they do not contradict, and integrate them coherently where \
they touch the same lines. Output ONLY the complete merged file content, with no conflict \
markers, no explanation, and no markdown code fences.";

/// Total input budget for AI conflict resolution, across all three versions.
/// Larger files must be resolved by hand; a truncated merge would corrupt
/// the file.
pub const MAX_MERGE_INPUT_CHARS: usize = 48_000;

/// Builds the user prompt for AI conflict resolution from the three stages.
/// Errs when the combined content exceeds [`MAX_MERGE_INPUT_CHARS`], because
/// truncating merge input would produce a corrupt file.
pub fn merge_prompt(
    path: &str,
    base: &str,
    ours: &str,
    theirs: &str,
    limit: usize,
) -> std::result::Result<String, String> {
    let total = base.len() + ours.len() + theirs.len();
    if total > limit {
        return Err(format!(
            "{path} is too large for AI resolution ({total} chars, limit {limit}). \
             Resolve it manually."
        ));
    }
    Ok(format!(
        "Resolve the merge conflict in `{path}`.\n\n\
         BASE (common ancestor):\n```\n{base}\n```\n\n\
         OURS (current branch):\n```\n{ours}\n```\n\n\
         THEIRS (incoming):\n```\n{theirs}\n```\n\n\
         Output the complete merged file content only."
    ))
}

/// Cleans model output into plain file content: trims a single wrapping
/// markdown code fence if present, preserving everything inside verbatim.
pub fn extract_merged_content(text: &str) -> String {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return ensure_trailing_newline(trimmed);
    };
    // Drop the info string (e.g. ```rust) on the fence line.
    let body = match rest.split_once('\n') {
        Some((_info, body)) => body,
        None => rest,
    };
    let body = body.strip_suffix("```").unwrap_or(body).trim_end_matches('\n');
    ensure_trailing_newline(body)
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// Truncates to at most `max` bytes on a char boundary, marking the cut.
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[diff truncated]", &s[..end])
}

/// Extracts a [`CommitSuggestion`] from model output, tolerating extra prose
/// around the JSON and falling back to first-line/rest splitting.
/// Shared with the Claude client.
pub fn parse_suggestion_text(text: &str) -> CommitSuggestion {
    parse_suggestion(text)
}

fn parse_suggestion(text: &str) -> CommitSuggestion {
    if let Ok(s) = serde_json::from_str::<CommitSuggestion>(text) {
        return clamp(s);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if end > start {
            if let Ok(s) = serde_json::from_str::<CommitSuggestion>(&text[start..=end]) {
                return clamp(s);
            }
        }
    }
    let mut lines = text.trim().lines();
    let summary = lines.next().unwrap_or("Update files").trim().to_string();
    let description = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    clamp(CommitSuggestion { summary, description })
}

/// Enforces the 72-char summary limit and trims whitespace.
fn clamp(mut s: CommitSuggestion) -> CommitSuggestion {
    s.summary = s.summary.trim().replace('\n', " ");
    if s.summary.chars().count() > 72 {
        s.summary = s.summary.chars().take(69).collect::<String>() + "...";
    }
    s.description = s.description.trim().to_string();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let s = parse_suggestion(r#"{"summary":"feat: add x","description":"details"}"#);
        assert_eq!(s.summary, "feat: add x");
        assert_eq!(s.description, "details");
    }

    #[test]
    fn parses_json_embedded_in_text() {
        let s = parse_suggestion("Sure:\n{\"summary\":\"fix: y\",\"description\":\"d\"}\ndone");
        assert_eq!(s.summary, "fix: y");
    }

    #[test]
    fn falls_back_to_line_split() {
        let s = parse_suggestion("fix bug in parser\nIt was broken.");
        assert_eq!(s.summary, "fix bug in parser");
        assert_eq!(s.description, "It was broken.");
    }

    #[test]
    fn clamps_long_summaries() {
        let long = "x".repeat(100);
        let s = parse_suggestion(&format!("{{\"summary\":\"{long}\",\"description\":\"\"}}"));
        assert!(s.summary.chars().count() <= 72);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(100); // 2 bytes each
        let t = truncate_utf8(&s, 51);
        assert!(t.contains("[diff truncated]"));
        assert!(t.starts_with('é'));
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merge_prompt_includes_all_three_versions() {
        let p = merge_prompt("a.rs", "b", "o", "t", 1000).unwrap();
        assert!(p.contains("BASE") && p.contains("OURS") && p.contains("THEIRS"));
        assert!(p.contains("a.rs"));
    }

    #[test]
    fn merge_prompt_rejects_oversized_input() {
        let big = "x".repeat(600);
        let err = merge_prompt("a.rs", &big, &big, &big, 1000).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn custom_instructions_extend_merge_prompt() {
        let s = merge_system_prompt(Some("Prefer tabs."));
        assert!(s.contains("resolving a git merge") && s.ends_with("Prefer tabs."));
        // Blank custom text leaves the built-in prompt untouched.
        assert_eq!(merge_system_prompt(Some("  ")), merge_system_prompt(None));
    }

    #[test]
    fn extract_strips_code_fence() {
        assert_eq!(extract_merged_content("```rust\nfn main() {}\n```"), "fn main() {}\n");
        assert_eq!(extract_merged_content("plain text"), "plain text\n");
        // Inner fences survive when there is no wrapping fence pair.
        assert_eq!(extract_merged_content("a\nb\n"), "a\nb\n");
    }
}
