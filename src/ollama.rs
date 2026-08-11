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
