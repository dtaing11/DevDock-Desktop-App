//! GitHub integration: device-flow sign-in, token storage, and the subset of
//! the REST API needed for pull requests.
//!
//! Authentication uses the OAuth device flow, the same mechanism as
//! `gh auth login`: the app shows a short code, the user enters it at
//! <https://github.com/login/device>, and the app polls for the token.
//! A personal access token can be used instead via [`TokenStore`].
//!
//! All requests are blocking; call them from a worker thread in GUI code.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// GitHub CLI's public OAuth client id (device flow enabled).
pub const DEFAULT_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";

const API_BASE: &str = "https://api.github.com";
const OAUTH_SCOPES: &str = "repo workflow";
const USER_AGENT: &str = "git-manage-linux/0.1";

/// Errors from GitHub requests. Messages are user-presentable.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GhError(pub String);

pub type Result<T> = std::result::Result<T, GhError>;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
}

fn read_json(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<serde_json::Value> {
    match resp {
        Ok(r) => r.into_json().map_err(|e| GhError(e.to_string())),
        Err(ureq::Error::Status(code, r)) => {
            let value: serde_json::Value = r.into_json().unwrap_or_default();
            let detail = value
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|a| a.first())
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| value.get("message").and_then(|m| m.as_str()))
                .unwrap_or("request failed");
            Err(GhError(format!("GitHub API {code}: {detail}")))
        }
        Err(e) => Err(GhError(format!("Cannot reach GitHub: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

/// Persists the OAuth/PAT token at `~/.config/git-manage/auth.json`,
/// AES-256-GCM encrypted with a key held in the OS keychain.
pub struct TokenStore;

impl TokenStore {
    fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("git-manage")
            .join("auth.json")
    }

    /// Saves the token, encrypted at rest (see [`crate::secure_store`]).
    pub fn save(token: &str) -> Result<()> {
        let json = serde_json::json!({ "token": token }).to_string();
        crate::secure_store::write(&Self::path(), &json).map_err(|e| GhError(e.to_string()))
    }

    /// Loads the stored token, if any.
    pub fn load() -> Option<String> {
        let data = crate::secure_store::read(&Self::path())?;
        serde_json::from_str::<serde_json::Value>(&data)
            .ok()?
            .get("token")?
            .as_str()
            .map(String::from)
    }

    /// Deletes the stored token.
    pub fn clear() -> Result<()> {
        crate::secure_store::remove(&Self::path()).map_err(|e| GhError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Device flow
// ---------------------------------------------------------------------------

/// Codes returned by GitHub to start a device-flow sign-in.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    pub verification_uri: String,
    /// Minimum seconds between polls.
    pub interval: u64,
    pub expires_in: u64,
}

/// Requests a device code to begin sign-in.
pub fn device_flow_start(client_id: &str) -> Result<DeviceCode> {
    let resp = agent()
        .post("https://github.com/login/device/code")
        .set("Accept", "application/json")
        .send_form(&[("client_id", client_id), ("scope", OAUTH_SCOPES)]);
    let value = read_json(resp)?;
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        return Err(GhError(format!("GitHub error: {error}")));
    }
    serde_json::from_value(value).map_err(|e| GhError(format!("Bad device-code response: {e}")))
}

/// Polls for the access token once.
///
/// Returns `Ok(Some(token))` once the user authorizes, `Ok(None)` while
/// authorization is still pending.
pub fn device_flow_poll(client_id: &str, device_code: &str) -> Result<Option<String>> {
    let resp = agent()
        .post("https://github.com/login/oauth/access_token")
        .set("Accept", "application/json")
        .send_form(&[
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
    let value = read_json(resp)?;

    if let Some(token) = value.get("access_token").and_then(|t| t.as_str()) {
        return Ok(Some(token.to_string()));
    }
    match value.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending" | "slow_down") | None => Ok(None),
        Some("expired_token") => Err(GhError("Sign-in code expired. Try again.".into())),
        Some("access_denied") => Err(GhError("Sign-in was denied.".into())),
        Some(other) => Err(GhError(format!("GitHub error: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// REST API
// ---------------------------------------------------------------------------

/// The authenticated GitHub user.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct User {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
}

/// An open pull request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub html_url: String,
    pub state: String,
    /// Source branch.
    pub head: String,
    /// SHA of the head commit (used for CI check lookups).
    pub head_sha: String,
    /// Target branch.
    pub base: String,
    /// Author login.
    pub user: String,
}

/// Aggregated CI (GitHub Actions / checks) state for one commit.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// All check runs finished successfully.
    Passing,
    /// At least one check run failed or was cancelled/timed out.
    Failing,
    /// Check runs exist but some are still queued or running.
    Pending,
    /// No check runs reported for this commit.
    None,
}

impl CheckState {
    /// Short human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Passing => "checks passing",
            Self::Failing => "checks failing",
            Self::Pending => "checks running",
            Self::None => "no checks",
        }
    }
}

/// Summary of the check runs on a commit.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChecksSummary {
    pub state: CheckState,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    /// Individual check runs, failures first.
    pub runs: Vec<CheckRun>,
}

/// One CI check run (e.g. a GitHub Actions job).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckRun {
    pub name: String,
    /// `queued`, `in_progress`, or `completed`.
    pub status: String,
    /// `success`, `failure`, `cancelled`, ... (empty while running).
    pub conclusion: String,
    /// Link to the run's page on GitHub.
    pub html_url: String,
}

/// A repository visible to the signed-in user (for the clone picker).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteRepo {
    pub full_name: String,
    pub clone_url: String,
    pub private: bool,
}

/// `owner/repo` pair identifying a GitHub repository.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug {
    pub owner: String,
    pub repo: String,
}

impl std::fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// Authenticated GitHub API client (blocking).
pub struct Client {
    token: String,
}

impl Client {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }

    /// Client using the stored token, if the user is signed in.
    pub fn from_store() -> Option<Self> {
        TokenStore::load().map(Self::new)
    }

    /// The token this client authenticates with.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Fetches the authenticated user's profile.
    pub fn user(&self) -> Result<User> {
        let value = self.get("/user")?;
        serde_json::from_value(value).map_err(|e| GhError(e.to_string()))
    }

    /// Lists open pull requests for a repository.
    pub fn pull_requests(&self, slug: &RepoSlug) -> Result<Vec<PullRequest>> {
        let path = format!("/repos/{}/{}/pulls?state=open&per_page=50", slug.owner, slug.repo);
        let value = self.get(&path)?;
        Ok(value
            .as_array()
            .map(|prs| prs.iter().filter_map(parse_pull_request).collect())
            .unwrap_or_default())
    }

    /// Opens a pull request from `head` into `base`.
    pub fn create_pull_request(
        &self,
        slug: &RepoSlug,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest> {
        let path = format!("/repos/{}/{}/pulls", slug.owner, slug.repo);
        let payload =
            serde_json::json!({ "title": title, "body": body, "head": head, "base": base });
        let value = self.post(&path, payload)?;
        parse_pull_request(&value).ok_or_else(|| GhError("Unexpected PR response".into()))
    }

    /// Summarizes GitHub Actions / check-run results for a commit or branch.
    ///
    /// `git_ref` may be a SHA, branch name, or tag.
    pub fn checks(&self, slug: &RepoSlug, git_ref: &str) -> Result<ChecksSummary> {
        let path = format!(
            "/repos/{}/{}/commits/{}/check-runs?per_page=100",
            slug.owner, slug.repo, git_ref
        );
        let value = self.get(&path)?;
        let runs = value
            .get("check_runs")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        let mut summary = ChecksSummary {
            state: CheckState::None,
            total: 0,
            passed: 0,
            failed: 0,
            pending: 0,
            runs: Vec::new(),
        };
        for run in &runs {
            summary.total += 1;
            let status = run.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let conclusion = run.get("conclusion").and_then(|c| c.as_str()).unwrap_or("");
            summary.runs.push(CheckRun {
                name: run
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("(unnamed check)")
                    .to_string(),
                status: status.to_string(),
                conclusion: conclusion.to_string(),
                html_url: run
                    .get("html_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
            if status != "completed" {
                summary.pending += 1;
            } else {
                match conclusion {
                    "success" | "neutral" | "skipped" => summary.passed += 1,
                    _ => summary.failed += 1,
                }
            }
        }
        // Failures first, then running, then passed, for scannability.
        summary.runs.sort_by_key(|r| match (r.status.as_str(), r.conclusion.as_str()) {
            ("completed", "success" | "neutral" | "skipped") => 2,
            ("completed", _) => 0,
            _ => 1,
        });
        summary.state = if summary.total == 0 {
            CheckState::None
        } else if summary.failed > 0 {
            CheckState::Failing
        } else if summary.pending > 0 {
            CheckState::Pending
        } else {
            CheckState::Passing
        };
        Ok(summary)
    }

    /// Repositories the user can access, most recently pushed first.
    pub fn my_repos(&self) -> Result<Vec<RemoteRepo>> {
        let value =
            self.get("/user/repos?sort=pushed&per_page=100&affiliation=owner,collaborator")?;
        Ok(value
            .as_array()
            .map(|repos| {
                repos
                    .iter()
                    .filter_map(|r| {
                        Some(RemoteRepo {
                            full_name: r.get("full_name")?.as_str()?.to_string(),
                            clone_url: r.get("clone_url")?.as_str()?.to_string(),
                            private: r.get("private")?.as_bool()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let resp = agent()
            .get(&format!("{API_BASE}{path}"))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/vnd.github+json")
            .call();
        read_json(resp)
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let resp = agent()
            .post(&format!("{API_BASE}{path}"))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/vnd.github+json")
            .send_json(body);
        read_json(resp)
    }
}

fn parse_pull_request(value: &serde_json::Value) -> Option<PullRequest> {
    Some(PullRequest {
        number: value.get("number")?.as_u64()?,
        title: value.get("title")?.as_str()?.to_string(),
        html_url: value.get("html_url")?.as_str()?.to_string(),
        state: value.get("state")?.as_str()?.to_string(),
        head: value.pointer("/head/ref")?.as_str()?.to_string(),
        head_sha: value.pointer("/head/sha")?.as_str()?.to_string(),
        base: value.pointer("/base/ref")?.as_str()?.to_string(),
        user: value.pointer("/user/login")?.as_str()?.to_string(),
    })
}

/// Parses `owner/repo` from HTTPS and SSH github.com remote URLs.
///
/// Returns `None` for URLs that do not point at github.com.
pub fn parse_remote(url: &str) -> Option<RepoSlug> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(RepoSlug { owner: owner.to_string(), repo: repo.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_remote() {
        let slug = parse_remote("https://github.com/foo/bar.git").unwrap();
        assert_eq!(slug, RepoSlug { owner: "foo".into(), repo: "bar".into() });
    }

    #[test]
    fn parses_ssh_remote() {
        let slug = parse_remote("git@github.com:foo/bar.git").unwrap();
        assert_eq!(slug.owner, "foo");
        assert_eq!(slug.repo, "bar");
    }

    #[test]
    fn parses_trailing_slash_without_dot_git() {
        let slug = parse_remote("https://github.com/foo/bar/").unwrap();
        assert_eq!(slug.repo, "bar");
    }

    #[test]
    fn rejects_non_github_hosts() {
        assert!(parse_remote("https://gitlab.com/foo/bar.git").is_none());
    }

    #[test]
    fn rejects_malformed_paths() {
        assert!(parse_remote("https://github.com/only-owner").is_none());
    }
}
