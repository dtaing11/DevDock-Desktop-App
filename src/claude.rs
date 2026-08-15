//! Claude (Anthropic) client: generates commit messages from diffs.
//!
//! Two ways to authenticate:
//! - **OAuth**: sign in with a claude.ai account (Pro/Max) using the same
//!   PKCE device flow as Claude Code. Tokens are stored and auto-refreshed.
//! - **API key**: from <https://console.anthropic.com>.
//!
//! Blocking HTTP; call from a worker thread in GUI code.

use crate::ollama::CommitSuggestion;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default model when none is chosen, Anthropic's fast/cheap tier.
/// Haiku also consumes the least subscription quota under OAuth.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// Static fallback list, used only when the models API is unreachable.
pub const FALLBACK_MODELS: &[&str] =
    &["claude-haiku-4-5-20251001", "claude-sonnet-4-5-20250929", "claude-opus-4-5-20251101"];

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const MAX_DIFF_CHARS: usize = 12_000;
const SYSTEM_PROMPT: &str = "You are an expert software engineer writing git commit messages. \
Given a diff, produce a concise conventional-commit style summary line (max 72 chars, imperative mood, \
e.g. 'feat: add user login') and a short description body explaining what changed and why. \
Write the description in GitHub-flavored Markdown (bullet lists, `code` spans, ### headings \
where they help readability). \
Respond only with JSON: {\"summary\": \"...\", \"description\": \"...\"}";

// Claude Code's public OAuth client (PKCE, manual code paste).
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

/// Errors from Claude requests. Messages are user-presentable.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ClaudeError(pub String);

pub type Result<T> = std::result::Result<T, ClaudeError>;

// ---------------------------------------------------------------------------
// Credential storage
// ---------------------------------------------------------------------------

/// Stored Claude credentials: an API key, OAuth tokens, or both.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub oauth: Option<OAuthTokens>,
}

/// OAuth access/refresh tokens with expiry (unix seconds).
#[derive(Serialize, Deserialize, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

/// Persists credentials at `~/.config/git-manage/claude.json`,
/// AES-256-GCM encrypted with a key held in the OS keychain.
pub struct CredentialStore;

impl CredentialStore {
    fn path() -> PathBuf {
        crate::secure_store::config_dir().join("claude.json")
    }

    pub fn load() -> Credentials {
        crate::secure_store::read(&Self::path())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(creds: &Credentials) -> Result<()> {
        let json = serde_json::to_string(creds).map_err(|e| ClaudeError(e.to_string()))?;
        crate::secure_store::write(&Self::path(), &json)
            .map_err(|e| ClaudeError(e.to_string()))
    }

    pub fn clear() -> Result<()> {
        crate::secure_store::remove(&Self::path()).map_err(|e| ClaudeError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// OAuth (PKCE, manual code paste like Claude Code)
// ---------------------------------------------------------------------------

/// Internal classification for retry decisions.
enum RequestError {
    /// Worth retrying after a wait (429 rate limits, 5xx).
    Transient { wait_hint: Option<u64>, error: ClaudeError },
    /// Retrying will not help (auth errors, bad requests).
    Fatal(ClaudeError),
}

/// An in-progress OAuth sign-in: open `url`, then exchange the pasted code.
#[derive(Clone)]
pub struct OAuthFlow {
    pub url: String,
    verifier: String,
    state: String,
}

/// Starts a PKCE flow. Open [`OAuthFlow::url`] in a browser; the user copies
/// the code shown after approval and passes it to [`finish_oauth`].
pub fn start_oauth() -> OAuthFlow {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let verifier: String = {
        let mut bytes = [0u8; 32];
        // Simple entropy source without extra deps: hash time + addresses.
        let seed = format!(
            "{:?}-{:p}-{}",
            SystemTime::now(),
            &bytes as *const _,
            std::process::id()
        );
        let digest = sha256(seed.as_bytes());
        bytes.copy_from_slice(&digest[..32]);
        b64.encode(bytes)
    };
    let challenge = b64.encode(sha256(verifier.as_bytes()));
    let state = b64.encode(sha256(format!("state-{verifier}").as_bytes()));

    let url = format!(
        "{OAUTH_AUTHORIZE_URL}?code=true&client_id={OAUTH_CLIENT_ID}&response_type=code\
         &redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        urlencode(OAUTH_REDIRECT_URI),
        urlencode(OAUTH_SCOPES),
    );
    OAuthFlow { url, verifier, state }
}

/// Exchanges the pasted `code` (formats: `code` or `code#state`) for tokens
/// and stores them alongside any existing API key.
pub fn finish_oauth(flow: &OAuthFlow, code: &str) -> Result<()> {
    let (code, state) = match code.trim().split_once('#') {
        Some((c, s)) => (c.to_string(), s.to_string()),
        None => (code.trim().to_string(), flow.state.clone()),
    };
    let payload = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "state": state,
        "client_id": OAUTH_CLIENT_ID,
        "redirect_uri": OAUTH_REDIRECT_URI,
        "code_verifier": flow.verifier,
    });
    let tokens = token_request(payload)?;
    let mut creds = CredentialStore::load();
    creds.oauth = Some(tokens);
    CredentialStore::save(&creds)
}

fn refresh_oauth(tokens: &OAuthTokens) -> Result<OAuthTokens> {
    let payload = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": tokens.refresh_token,
        "client_id": OAUTH_CLIENT_ID,
    });
    token_request(payload)
}

fn token_request(payload: serde_json::Value) -> Result<OAuthTokens> {
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .post(OAUTH_TOKEN_URL)
        .set("content-type", "application/json")
        .send_json(payload);
    let value: serde_json::Value = match resp {
        Ok(r) => r.into_json().map_err(|e| ClaudeError(e.to_string()))?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(ClaudeError(format!("Claude OAuth error {code}: {body}")));
        }
        Err(e) => return Err(ClaudeError(format!("Cannot reach Claude: {e}"))),
    };
    let access = value
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| ClaudeError("No access token in response".into()))?;
    let refresh = value
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let expires_in = value.get("expires_in").and_then(|t| t.as_u64()).unwrap_or(3600);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Ok(OAuthTokens {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at: now + expires_in.saturating_sub(60),
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// How the client authenticates.
enum Auth {
    ApiKey(String),
    OAuth(OAuthTokens),
}

/// Client for the Anthropic Messages API (blocking).
pub struct Client {
    auth: Auth,
    model: String,
}

impl Client {
    /// Client with an explicit API key.
    pub fn with_api_key(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self { auth: Auth::ApiKey(api_key.into()), model: normalize_model(model) }
    }

    /// Client from stored credentials, preferring OAuth (refreshing when
    /// expired) and falling back to a stored API key.
    pub fn from_store(model: impl Into<String>) -> Option<Self> {
        let mut creds = CredentialStore::load();
        if let Some(tokens) = creds.oauth.clone() {
            let now =
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let tokens = if now >= tokens.expires_at && !tokens.refresh_token.is_empty() {
                match refresh_oauth(&tokens) {
                    Ok(t) => {
                        creds.oauth = Some(t.clone());
                        let _ = CredentialStore::save(&creds);
                        t
                    }
                    Err(_) => tokens, // try the stale token; API will say no
                }
            } else {
                tokens
            };
            return Some(Self { auth: Auth::OAuth(tokens), model: normalize_model(model) });
        }
        creds
            .api_key
            .map(|key| Self { auth: Auth::ApiKey(key), model: normalize_model(model) })
    }

    /// Which sign-in method the stored credentials use, for the UI.
    pub fn auth_label() -> Option<&'static str> {
        let creds = CredentialStore::load();
        if creds.oauth.is_some() {
            Some("claude.ai account (OAuth)")
        } else if creds.api_key.is_some() {
            Some("API key")
        } else {
            None
        }
    }

    /// Cheap connectivity/authentication check.
    pub fn verify(&self) -> Result<()> {
        self.request("Say OK", 16).map(drop)
    }

    /// Models actually available to this account, from `/v1/models`.
    /// Falls back to [`FALLBACK_MODELS`] when the endpoint is unavailable.
    pub fn models(&self) -> Vec<String> {
        let mut req = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .build()
            .get("https://api.anthropic.com/v1/models?limit=50")
            .set("anthropic-version", "2023-06-01");
        req = match &self.auth {
            Auth::ApiKey(key) => req.set("x-api-key", key),
            Auth::OAuth(tokens) => req
                .set("Authorization", &format!("Bearer {}", tokens.access_token))
                .set("anthropic-beta", "oauth-2025-04-20"),
        };
        let models: Vec<String> = req
            .call()
            .ok()
            .and_then(|r| r.into_json::<serde_json::Value>().ok())
            .and_then(|v| {
                v.get("data")?.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id")?.as_str().map(String::from))
                        .collect()
                })
            })
            .unwrap_or_default();
        if models.is_empty() {
            FALLBACK_MODELS.iter().map(|s| s.to_string()).collect()
        } else {
            models
        }
    }

    /// Generates a commit message for `diff`.
    ///
    /// `extra_instructions` is appended to the system prompt for per-repo
    /// style customization.
    pub fn commit_message(
        &self,
        diff: &str,
        extra_instructions: Option<&str>,
    ) -> Result<CommitSuggestion> {
        if diff.trim().is_empty() {
            return Err(ClaudeError(
                "No changes to describe. Stage or modify some files first.".into(),
            ));
        }
        let system = match extra_instructions.filter(|s| !s.trim().is_empty()) {
            Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
            None => SYSTEM_PROMPT.to_string(),
        };
        let truncated = truncate_utf8(diff, MAX_DIFF_CHARS);
        let prompt =
            format!("Write a commit message for this diff:\n\n```diff\n{truncated}\n```");
        let text = self.request_with_system(&system, &prompt, 1024)?;
        Ok(crate::ollama::parse_suggestion_text(&text))
    }

    /// Asks Claude to merge a conflicted file from its three stages.
    /// Returns the full merged file content.
    pub fn resolve_conflict(
        &self,
        path: &str,
        base: &str,
        ours: &str,
        theirs: &str,
        extra_instructions: Option<&str>,
    ) -> Result<String> {
        // Claude's smaller budget: the merge output must reproduce the whole
        // file, so cap input well below the commit-diff budget logic.
        let prompt = crate::ollama::merge_prompt(path, base, ours, theirs, 24_000)
            .map_err(ClaudeError)?;
        let system = crate::ollama::merge_system_prompt(extra_instructions);
        let text = self.request_with_system(&system, &prompt, 8192)?;
        Ok(crate::ollama::extract_merged_content(&text))
    }

    /// Asks Claude to draft a `.git-manage-ci.toml` from a repo scan.
    /// Returns raw TOML text (a proposal for the user to review).
    pub fn generate_ci_config(&self, repo_scan: &str) -> Result<String> {
        let prompt = format!(
            "Write .git-manage-ci.toml for this repository:\n\n{}",
            truncate_utf8(repo_scan, MAX_DIFF_CHARS)
        );
        let text = self.request_with_system(
            crate::local_ci::AI_CONFIG_SYSTEM_PROMPT,
            &prompt,
            2048,
        )?;
        Ok(crate::ollama::extract_merged_content(&text))
    }

    fn request(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        self.request_with_system(SYSTEM_PROMPT, prompt, max_tokens)
    }

    /// Sends one request with automatic retry on transient failures and
    /// automatic model fallback on persistent rate limits.
    ///
    /// Claude subscriptions cap Opus/Sonnet usage separately from (and far
    /// lower than) Haiku. When the selected model keeps returning 429, we
    /// silently fall back to the newest Haiku so generation still succeeds
    /// on subscription auth, instead of failing until the weekly window
    /// resets. Transient 429/5xx also get bounded backoff retries.
    fn request_with_system(
        &self,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String> {
        /// Backoff retries. Waits are short because model fallback (not
        /// waiting) is the real fix for persistent subscription limits.
        const MAX_RETRIES: u32 = 2;
        const MAX_WAIT_SECS: u64 = 15;

        let mut model = self.model.clone();
        let mut fell_back = false;
        let mut attempt = 0;
        loop {
            match self.request_once(&model, system, prompt, max_tokens) {
                Err(RequestError::Transient { wait_hint, error }) => {
                    // First 429 on a non-Haiku model: switch to Haiku
                    // immediately. Opus/Sonnet caps are weekly, so backoff
                    // cannot clear them, but Haiku's cap is far higher.
                    if !fell_back && !model.contains("haiku") {
                        if let Some(haiku) = self.newest_haiku() {
                            model = haiku;
                            fell_back = true;
                            attempt = 0;
                            continue;
                        }
                    }
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(error);
                    }
                    let wait = wait_hint
                        .unwrap_or(2 * 3u64.pow(attempt - 1))
                        .min(MAX_WAIT_SECS);
                    std::thread::sleep(Duration::from_secs(wait));
                }
                Err(RequestError::Fatal(e)) => return Err(e),
                Ok(text) => return Ok(text),
            }
        }
    }

    /// Newest Haiku model id available to this account, for rate-limit
    /// fallback. Uses the live model list so ids never go stale.
    fn newest_haiku(&self) -> Option<String> {
        self.models().into_iter().find(|m| m.contains("haiku"))
    }

    fn request_once(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> std::result::Result<String, RequestError> {
        #[derive(Deserialize)]
        struct Content {
            text: Option<String>,
        }
        #[derive(Deserialize)]
        struct Response {
            content: Vec<Content>,
        }

        let payload = serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": prompt}],
        });
        let mut req = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(120))
            .build()
            .post(API_URL)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json");
        req = match &self.auth {
            Auth::ApiKey(key) => req.set("x-api-key", key),
            Auth::OAuth(tokens) => req
                .set("Authorization", &format!("Bearer {}", tokens.access_token))
                .set("anthropic-beta", "oauth-2025-04-20"),
        };
        let resp = req.send_json(payload);

        let value: serde_json::Value = match resp {
            Ok(r) => r
                .into_json()
                .map_err(|e| RequestError::Fatal(ClaudeError(e.to_string())))?,
            Err(ureq::Error::Status(code, r)) => {
                let retry_after: Option<u64> =
                    r.header("retry-after").and_then(|v| v.parse().ok());
                let body: serde_json::Value = r.into_json().unwrap_or_default();
                let detail = body
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("request failed");
                // 429 and 5xx are transient: retried automatically with
                // backoff before this error ever reaches the user.
                if code == 429 || code >= 500 {
                    let error = ClaudeError(format!(
                        "Claude API {code} on {model}: {detail} (auto-retried \
                         and tried falling back to Haiku. Subscriptions cap \
                         Opus/Sonnet separately; pick Haiku in the model \
                         picker or wait for the window to reset)"
                    ));
                    return Err(RequestError::Transient { wait_hint: retry_after, error });
                }
                let hint = match code {
                    401 => " (sign in again in Settings)",
                    _ => "",
                };
                return Err(RequestError::Fatal(ClaudeError(format!(
                    "Claude API {code}: {detail}{hint}"
                ))));
            }
            Err(e) => {
                return Err(RequestError::Fatal(ClaudeError(format!(
                    "Cannot reach Claude: {e}"
                ))))
            }
        };
        let parsed: Response = serde_json::from_value(value)
            .map_err(|e| RequestError::Fatal(ClaudeError(format!("Bad response: {e}"))))?;
        parsed
            .content
            .into_iter()
            .filter_map(|c| c.text)
            .next()
            .ok_or_else(|| RequestError::Fatal(ClaudeError("Claude returned no text".into())))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_model(model: impl Into<String>) -> String {
    let model = model.into();
    if model.trim().is_empty() { DEFAULT_MODEL.to_string() } else { model }
}

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

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_defaults_when_blank() {
        assert_eq!(normalize_model("  "), DEFAULT_MODEL);
        assert_eq!(normalize_model("claude-sonnet-4-5"), "claude-sonnet-4-5");
    }

    #[test]
    fn oauth_url_contains_pkce_params() {
        let flow = start_oauth();
        assert!(flow.url.starts_with(OAUTH_AUTHORIZE_URL));
        assert!(flow.url.contains("code_challenge="));
        assert!(flow.url.contains("code_challenge_method=S256"));
        assert!(flow.url.contains(OAUTH_CLIENT_ID));
        assert!(!flow.verifier.is_empty());
    }

    #[test]
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b:c/d"), "a%20b%3Ac%2Fd");
    }
}
