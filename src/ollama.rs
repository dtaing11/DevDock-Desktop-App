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

    /// Writes a pull request title and body from a branch summary.
    pub fn pull_request_text(
        &self,
        model: &str,
        summary: &crate::git::BranchSummary,
        extra_instructions: Option<&str>,
    ) -> Result<CommitSuggestion> {
        if summary.is_empty() {
            return Err(OllamaError(
                "Nothing to describe: this branch has no commits the base does not.".into(),
            ));
        }
        let payload = serde_json::json!({
            "model": model,
            "prompt": pr_prompt(summary, MAX_DIFF_CHARS),
            "system": pr_system_prompt(extra_instructions),
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

// ---------------------------------------------------------------------------
// Tool-use harness
// ---------------------------------------------------------------------------

/// Context window requested for agentic runs. Ollama defaults to a window
/// far too small to hold a transcript of file reads, and silently drops the
/// oldest messages when it overflows — which looks like a model that forgot
/// what it just read.
const AGENT_NUM_CTX: u32 = 16_384;

/// One Ollama model bound to one server, driven through the tool-use loop
/// in [`crate::agent`]. Created with [`Client::agent`].
///
/// Tool calling is a per-model capability: `qwen3`, `llama3.1`+, `mistral`,
/// and the coder models support it, while many smaller or older tags do
/// not. When the server says so, [`crate::agent::Provider::turn`] returns a
/// message naming the fix rather than a raw API error.
pub struct Agent {
    client: Client,
    model: String,
}

impl Client {
    /// Binds this server to one model for an agentic run.
    pub fn agent(&self, model: impl Into<String>) -> Agent {
        Agent { client: self.clone(), model: model.into() }
    }
}

impl crate::agent::Provider for Agent {
    fn label(&self) -> String {
        format!("Ollama ({})", self.model)
    }

    fn turn(
        &self,
        system: &str,
        messages: &[crate::agent::Message],
        tools: &[crate::agent::ToolSpec],
        max_tokens: u32,
    ) -> std::result::Result<crate::agent::Reply, String> {
        let mut wire = vec![serde_json::json!({"role": "system", "content": system})];
        wire.extend(wire_messages(messages));

        let mut payload = serde_json::json!({
            "model": self.model,
            "messages": wire,
            "stream": false,
            "options": {
                "temperature": 0.0,
                "num_ctx": AGENT_NUM_CTX,
                "num_predict": max_tokens,
            },
        });
        if !tools.is_empty() {
            payload["tools"] = serde_json::Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.schema,
                            }
                        })
                    })
                    .collect(),
            );
        }

        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(600))
            .build()
            .post(&format!("{}/api/chat", self.client.base_url))
            .send_json(payload);

        let value: serde_json::Value = match resp {
            Ok(r) => r.into_json().map_err(|e| format!("Bad response from Ollama: {e}"))?,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                if body.contains("does not support tools") {
                    return Err(format!(
                        "{} cannot call tools, so it cannot read the repository. Pick a \
                         tool-capable local model (qwen3, llama3.1 or newer, mistral, the \
                         coder tags) or switch this task to Claude.",
                        self.model
                    ));
                }
                return Err(format!("Ollama error {code}: {body}"));
            }
            Err(e) => return Err(format!("Ollama request failed: {e}")),
        };

        Ok(parse_agent_reply(&value))
    }
}

/// Rewrites the harness transcript into Ollama's chat format. Ollama has no
/// tool-call ids, matching results to calls by position, so the ids the
/// harness assigned are dropped here and restored by the caller.
fn wire_messages(messages: &[crate::agent::Message]) -> Vec<serde_json::Value> {
    use crate::agent::Message;
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::User(text) => {
                out.push(serde_json::json!({"role": "user", "content": text}));
            }
            Message::Assistant { text, calls } => {
                let tool_calls: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "function": {"name": c.name, "arguments": c.input}
                        })
                    })
                    .collect();
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": text,
                    "tool_calls": tool_calls,
                }));
            }
            Message::ToolResults(results) => {
                for r in results {
                    let content = if r.is_error {
                        format!("ERROR: {}", r.content)
                    } else {
                        r.content.clone()
                    };
                    out.push(serde_json::json!({
                        "role": "tool",
                        "tool_name": r.name,
                        "content": content,
                    }));
                }
            }
        }
    }
    out
}

/// Pulls text and tool calls out of an `/api/chat` response.
fn parse_agent_reply(value: &serde_json::Value) -> crate::agent::Reply {
    use crate::agent::{Reply, ToolCall};
    let mut reply = Reply::default();
    let Some(message) = value.get("message") else { return reply };
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        reply.text = text.to_string();
    }
    let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) else {
        return reply;
    };
    for (i, call) in calls.iter().enumerate() {
        let Some(function) = call.get("function") else { continue };
        let Some(name) = function.get("name").and_then(|n| n.as_str()) else { continue };
        // Arguments arrive as an object from most models and as a JSON
        // string from a few; accept either rather than dropping the call.
        let input = match function.get("arguments") {
            Some(serde_json::Value::String(raw)) => {
                serde_json::from_str(raw).unwrap_or(serde_json::json!({}))
            }
            Some(other) => other.clone(),
            None => serde_json::json!({}),
        };
        reply.calls.push(ToolCall {
            id: format!("{name}-{i}"),
            name: name.to_string(),
            input,
        });
    }
    reply
}

// ---------------------------------------------------------------------------
// Pull request text
// ---------------------------------------------------------------------------

const PR_SYSTEM_PROMPT: &str = "You are writing the title and description of a pull request, \
from the commits on a branch and the diff they add up to.\n\n\
Title: one line, under 72 characters, imperative mood, describing the branch as a whole. If the \
branch does one thing, name that thing rather than listing every commit.\n\n\
Description: GitHub-flavoured Markdown, written for the person who has to review it. Say what \
changed and why, grouping related commits into themes instead of transcribing the log — the \
reviewer can read the log. Lead with the part that matters. Call out anything that needs \
attention: a behaviour change, a migration, a deliberate omission, something you cannot tell \
from the diff alone.\n\n\
Do not invent issue numbers, ticket links, reviewers, or test results. Do not add empty \
headings for sections you have nothing to say for. Do not describe the change as \
\"comprehensive\" or \"robust\"; describe what it does.\n\n\
Respond only with JSON: {\"summary\": \"the title\", \"description\": \"the body\"}";

/// The pull request system prompt, with the repository's own instructions
/// appended. Custom text extends the built-in rules rather than replacing
/// them, so the JSON contract stays intact.
pub fn pr_system_prompt(extra_instructions: Option<&str>) -> String {
    match extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{PR_SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => PR_SYSTEM_PROMPT.to_string(),
    }
}

/// How much of a commit body to carry into the prompt. The "why" is usually
/// in the first paragraph; a long body is a document of its own.
const MAX_BODY_CHARS: usize = 400;

/// Builds the user turn for pull request text: the branch, its commits, the
/// diffstat, and as much of the diff as the budget allows.
///
/// The commits come first and are never truncated. A branch's log is the
/// cheapest, densest description of its intent that exists — losing it to
/// make room for more diff would be exactly the wrong trade.
pub fn pr_prompt(summary: &crate::git::BranchSummary, max_diff_chars: usize) -> String {
    let mut prompt = String::new();

    let target = if summary.base.trim().is_empty() {
        "its upstream".to_string()
    } else {
        format!("`{}`", summary.base.trim())
    };
    prompt.push_str(&format!(
        "Pull request from `{}` into {target}.\n\n",
        if summary.branch.is_empty() { "this branch" } else { &summary.branch }
    ));

    match summary.commits.len() {
        0 => prompt.push_str("No commits were found on this branch.\n\n"),
        // Oldest first: that is the order the work happened in, and the
        // order the description should follow.
        n => {
            prompt.push_str(&format!("{n} commit(s), oldest first:\n"));
            for commit in summary.commits.iter().rev() {
                prompt.push_str(&format!("\n- {}", commit.subject));
                let body = commit.body.trim();
                if !body.is_empty() {
                    let body = truncate_utf8(body, MAX_BODY_CHARS);
                    for line in body.lines() {
                        prompt.push_str(&format!("\n  {line}"));
                    }
                }
            }
            prompt.push_str("\n\n");
        }
    }

    if !summary.stat.trim().is_empty() {
        prompt.push_str(&format!("Files changed:\n{}\n\n", summary.stat.trim()));
    }

    if summary.diff.trim().is_empty() {
        prompt.push_str("The diff is empty.");
    } else {
        prompt.push_str(&format!(
            "The branch's full diff:\n\n```diff\n{}\n```",
            truncate_utf8(&summary.diff, max_diff_chars)
        ));
    }
    prompt
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

#[cfg(test)]
mod pr_tests {
    use super::*;

    fn summary_fixture() -> crate::git::BranchSummary {
        let commit = |subject: &str, body: &str| crate::git::Commit {
            sha: "0".repeat(40),
            short_sha: "0000000".into(),
            author: "Tester".into(),
            email: "t@t.io".into(),
            date: "2026-01-01T00:00:00Z".into(),
            subject: subject.into(),
            body: body.into(),
            parents: Vec::new(),
            refs: Vec::new(),
        };
        crate::git::BranchSummary {
            branch: "feat/flags".into(),
            base: "main".into(),
            // Newest first, the way git log reports it.
            commits: vec![
                commit("feat: document the flag", ""),
                commit("feat: add --json", "Scripts had to parse the human output."),
            ],
            stat: " src/cli.rs | 20 ++++++++\n 1 file changed".into(),
            diff: "diff --git a/src/cli.rs b/src/cli.rs\n+let json = true;\n".into(),
        }
    }

    #[test]
    fn the_pr_prompt_leads_with_the_commits_oldest_first() {
        let prompt = pr_prompt(&summary_fixture(), 10_000);
        let older = prompt.find("feat: add --json").unwrap();
        let newer = prompt.find("feat: document the flag").unwrap();
        assert!(older < newer, "commits should read in the order the work happened");

        // The branch, the base, the reasoning from a commit body, the stat,
        // and the diff all have to reach the model.
        assert!(prompt.contains("`feat/flags`") && prompt.contains("`main`"));
        assert!(prompt.contains("Scripts had to parse the human output."));
        assert!(prompt.contains("src/cli.rs | 20"));
        assert!(prompt.contains("+let json = true;"));
    }

    #[test]
    fn the_pr_prompt_keeps_every_commit_when_the_diff_is_truncated() {
        let mut summary = summary_fixture();
        summary.diff = "x".repeat(50_000);
        let prompt = pr_prompt(&summary, 1_000);
        assert!(prompt.contains("feat: add --json"), "commits must survive truncation");
        assert!(prompt.contains("feat: document the flag"));
        assert!(prompt.contains("[diff truncated]"));
        assert!(prompt.len() < 5_000, "the diff should have been cut: {} chars", prompt.len());
    }

    #[test]
    fn the_pr_prompt_says_so_when_there_is_nothing_to_go_on() {
        let empty = crate::git::BranchSummary {
            branch: "feat/x".into(),
            base: String::new(),
            ..Default::default()
        };
        let prompt = pr_prompt(&empty, 1_000);
        assert!(prompt.contains("No commits"));
        assert!(prompt.contains("The diff is empty."));
        // With no base named, it says what it actually compared against.
        assert!(prompt.contains("its upstream"));
    }

    #[test]
    fn the_pr_system_prompt_extends_rather_than_replaces() {
        let base = pr_system_prompt(None);
        assert!(base.contains("pull request"));
        assert!(base.contains("summary"));
        let extended = pr_system_prompt(Some("Always link the Jira ticket."));
        assert!(extended.starts_with(&base));
        assert!(extended.contains("Always link the Jira ticket."));
        assert_eq!(pr_system_prompt(Some("   ")), base);
    }
}
