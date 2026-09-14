//! Jira Cloud: credentials, projects, and creating issues.
//!
//! Enough of the REST API to turn a list of work into tickets and to work a
//! backlog: check who you are, list the projects you can file into, list a
//! project's issue types, create issues, read the unassigned issues of a
//! project, and claim one — assign it, put it in the active sprint, move it
//! to In Progress, comment on it. The reading and claiming exist for
//! [`crate::backlog`], which picks tickets an agent can take; there is
//! still no attempt to be a Jira browser — the one in the browser is
//! better at it.
//!
//! # Authentication
//!
//! Basic auth with an email address and an API token, which is what Atlassian
//! issues for Cloud (`id.atlassian.com/manage-profile/security/api-tokens`).
//! The token is stored the way every other credential here is: encrypted at
//! rest by [`crate::secure_store`].
//!
//! # Descriptions
//!
//! v3 of the API does not take text. A description is an Atlassian Document
//! Format tree, so [`to_adf`] converts the Markdown a model writes into one.
//! Getting this wrong is the difference between a ticket and a 400.

use serde::{Deserialize, Serialize};

/// Errors from the Jira API.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct JiraError(pub String);

pub type Result<T> = std::result::Result<T, JiraError>;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Where the site is and who is talking to it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Credentials {
    /// Base URL, e.g. `https://acme.atlassian.net`.
    pub site: String,
    pub email: String,
    pub token: String,
}

impl Credentials {
    /// Builds credentials, accepting a site in any of the forms people paste.
    ///
    /// `acme`, `acme.atlassian.net`, `https://acme.atlassian.net`, and one
    /// with a trailing slash or a path glued on all name the same site, and
    /// asking someone to work out which one is meant is a support ticket
    /// waiting to happen.
    pub fn new(site: &str, email: &str, token: &str) -> Self {
        Self {
            site: normalise_site(site),
            email: email.trim().to_string(),
            token: token.trim().to_string(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.site.is_empty() && !self.email.is_empty() && !self.token.is_empty()
    }

    fn header(&self) -> String {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.email, self.token));
        format!("Basic {basic}")
    }
}

/// Turns whatever someone pasted into `https://<host>`.
pub fn normalise_site(site: &str) -> String {
    let s = site.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    // Anything after the host is a path into the UI, not part of the site.
    let host = s.split('/').next().unwrap_or(s).trim();
    if host.is_empty() {
        return String::new();
    }
    // A bare name is a Cloud subdomain; anything with a dot is already a host.
    if host.contains('.') {
        format!("https://{host}")
    } else {
        format!("https://{host}.atlassian.net")
    }
}

/// Persists the credentials, encrypted at rest.
pub struct CredentialStore;

impl CredentialStore {
    fn path() -> std::path::PathBuf {
        crate::secure_store::config_dir().join("jira.json")
    }

    pub fn save(creds: &Credentials) -> Result<()> {
        let json = serde_json::to_string(creds).map_err(|e| JiraError(e.to_string()))?;
        crate::secure_store::write(&Self::path(), &json).map_err(|e| JiraError(e.to_string()))
    }

    pub fn load() -> Option<Credentials> {
        let data = crate::secure_store::read(&Self::path())?;
        serde_json::from_str::<Credentials>(&data).ok().filter(Credentials::is_complete)
    }

    pub fn clear() -> Result<()> {
        crate::secure_store::remove(&Self::path()).map_err(|e| JiraError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// The authenticated account.
#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: String,
    pub display_name: String,
    pub email: String,
}

/// A project issues can be filed into.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub key: String,
    pub name: String,
}

/// One issue type in a project ("Task", "Bug", "Story").
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IssueType {
    pub id: String,
    pub name: String,
    /// Sub-tasks need a parent, so they are not offered on their own.
    pub subtask: bool,
}

/// An issue that now exists.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub key: String,
    /// Where a person can open it.
    pub url: String,
}

/// An issue read from a backlog: what a person would see at the top of it.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct BacklogIssue {
    pub id: String,
    pub key: String,
    pub summary: String,
    /// The description as plain text, from its Atlassian Document Format.
    pub description: String,
    pub issue_type: String,
    pub priority: String,
    pub status: String,
    pub labels: Vec<String>,
    /// ISO-8601, as Jira reports it.
    pub updated: String,
    pub url: String,
}

impl BacklogIssue {
    /// The ticket as a block of text for a prompt: key, summary, and the
    /// description, capped so one novel of a ticket cannot crowd out the
    /// rest.
    pub fn prompt_text(&self, max_description: usize) -> String {
        let mut description = self.description.trim().to_string();
        if description.len() > max_description {
            let end = (0..=max_description).rev().find(|i| description.is_char_boundary(*i)).unwrap_or(0);
            description.truncate(end);
            description.push_str("\n[truncated]");
        }
        let mut text = format!("{}: {}", self.key, self.summary.trim());
        let mut meta = Vec::new();
        if !self.issue_type.is_empty() {
            meta.push(format!("type: {}", self.issue_type));
        }
        if !self.priority.is_empty() {
            meta.push(format!("priority: {}", self.priority));
        }
        if !self.labels.is_empty() {
            meta.push(format!("labels: {}", self.labels.join(", ")));
        }
        if !meta.is_empty() {
            text.push_str(&format!(" ({})", meta.join("; ")));
        }
        if !description.is_empty() {
            text.push('\n');
            text.push_str(&description);
        }
        text
    }
}

/// A sprint on a scrum board.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Sprint {
    pub id: u64,
    pub name: String,
}

/// A workflow step an issue can take right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub id: String,
    pub name: String,
    /// The status category it leads to: "new", "indeterminate" (in
    /// progress), or "done".
    pub to_category: String,
}

/// An issue to create.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct NewIssue {
    pub project_key: String,
    /// Issue type by name, as the project spells it.
    pub issue_type: String,
    pub summary: String,
    /// Markdown; converted to Atlassian Document Format on the way out.
    pub description: String,
    pub labels: Vec<String>,
    /// Parent issue key, for a sub-task or an epic's child.
    pub parent: Option<String>,
}

impl NewIssue {
    /// The `fields` object this issue becomes.
    ///
    /// Separate from the request so it can be tested without a Jira to send
    /// it to: nearly everything that goes wrong here is the shape of this
    /// object rather than the HTTP around it.
    pub fn fields(&self) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "project".into(),
            serde_json::json!({ "key": self.project_key.trim() }),
        );
        fields.insert("summary".into(), self.summary.trim().into());
        fields.insert(
            "issuetype".into(),
            serde_json::json!({ "name": self.issue_type.trim() }),
        );
        if !self.description.trim().is_empty() {
            fields.insert("description".into(), to_adf(&self.description));
        }
        // Jira rejects a label containing a space, which is exactly what a
        // model writes when asked for labels. Fixing it here beats a 400 the
        // user cannot act on.
        let labels: Vec<String> = self
            .labels
            .iter()
            .map(|l| l.trim().replace(char::is_whitespace, "-"))
            .filter(|l| !l.is_empty())
            .collect();
        if !labels.is_empty() {
            fields.insert("labels".into(), labels.into());
        }
        if let Some(parent) = self.parent.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            fields.insert("parent".into(), serde_json::json!({ "key": parent }));
        }
        serde_json::Value::Object(fields)
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Authenticated Jira Cloud client (blocking).
pub struct Client {
    creds: Credentials,
}

impl Client {
    pub fn new(creds: Credentials) -> Self {
        Self { creds }
    }

    /// Client from the stored credentials, if there are any.
    pub fn from_store() -> Option<Self> {
        CredentialStore::load().map(Self::new)
    }

    pub fn site(&self) -> &str {
        &self.creds.site
    }

    /// Where a person opens an issue key.
    pub fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{}", self.creds.site, key.trim())
    }

    /// Who the credentials belong to. The cheapest way to tell a good token
    /// from a bad one.
    pub fn myself(&self) -> Result<Account> {
        let value = self.get("/rest/api/3/myself")?;
        Ok(Account {
            account_id: string_at(&value, "accountId"),
            display_name: string_at(&value, "displayName"),
            email: string_at(&value, "emailAddress"),
        })
    }

    /// Projects the account can see, newest-used first as Jira orders them.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let value = self.get("/rest/api/3/project/search?maxResults=100&orderBy=lastIssueUpdatedTime")?;
        Ok(value
            .get("values")
            .and_then(|v| v.as_array())
            .map(|values| values.iter().filter_map(parse_project).collect())
            .unwrap_or_default())
    }

    /// The issue types a project accepts.
    pub fn issue_types(&self, project: &str) -> Result<Vec<IssueType>> {
        let path = format!(
            "/rest/api/3/issue/createmeta/{}/issuetypes?maxResults=100",
            project.trim()
        );
        let value = self.get(&path)?;
        Ok(value
            .get("issueTypes")
            .or_else(|| value.get("values"))
            .and_then(|v| v.as_array())
            .map(|types| types.iter().filter_map(parse_issue_type).collect())
            .unwrap_or_default())
    }

    /// Creates one issue.
    pub fn create_issue(&self, issue: &NewIssue) -> Result<Issue> {
        if issue.summary.trim().is_empty() {
            return Err(JiraError("an issue needs a summary".into()));
        }
        let payload = serde_json::json!({ "fields": issue.fields() });
        let value = self.post("/rest/api/3/issue", payload)?;
        let key = string_at(&value, "key");
        if key.is_empty() {
            return Err(JiraError("Jira did not return an issue key".into()));
        }
        Ok(Issue { id: string_at(&value, "id"), url: self.browse_url(&key), key })
    }

    /// The unassigned, unresolved issues of a project: the backlog nobody has
    /// picked up, highest priority first.
    pub fn unassigned_backlog(&self, project: &str, max: usize) -> Result<Vec<BacklogIssue>> {
        self.search(&backlog_jql(project), max)
    }

    /// Issues matching a JQL query, newest API first with the older one as a
    /// fallback: Cloud sites moved to `/search/jql` in 2025 and the old
    /// endpoint answers 404 or 410 once it is gone.
    pub fn search(&self, jql: &str, max: usize) -> Result<Vec<BacklogIssue>> {
        let fields = "summary,description,issuetype,priority,status,labels,updated";
        let max = max.clamp(1, 100);
        let query = format!(
            "jql={}&maxResults={max}&fields={}",
            percent_encode(jql),
            percent_encode(fields)
        );
        let value = match self.get(&format!("/rest/api/3/search/jql?{query}")) {
            Ok(v) => v,
            Err(JiraError(e)) if e.contains("Jira error 404") || e.contains("Jira error 410") => {
                self.get(&format!("/rest/api/3/search?{query}"))?
            }
            Err(e) => return Err(e),
        };
        Ok(value
            .get("issues")
            .and_then(|v| v.as_array())
            .map(|issues| issues.iter().filter_map(|i| parse_backlog_issue(i, &self.creds.site)).collect())
            .unwrap_or_default())
    }

    /// Assigns an issue to an account.
    pub fn assign(&self, key: &str, account_id: &str) -> Result<()> {
        self.put_empty(
            &format!("/rest/api/3/issue/{}/assignee", key.trim()),
            serde_json::json!({ "accountId": account_id }),
        )
    }

    /// The active sprint of the project's first scrum board, if it has one.
    /// A kanban project has no sprints, which is `None`, not an error.
    pub fn active_sprint(&self, project: &str) -> Result<Option<Sprint>> {
        let boards = self.get(&format!(
            "/rest/agile/1.0/board?projectKeyOrId={}&type=scrum&maxResults=20",
            percent_encode(project.trim())
        ))?;
        let ids: Vec<u64> = boards
            .get("values")
            .and_then(|v| v.as_array())
            .map(|b| b.iter().filter_map(|x| x.get("id").and_then(|i| i.as_u64())).collect())
            .unwrap_or_default();
        for id in ids {
            let sprints = self.get(&format!("/rest/agile/1.0/board/{id}/sprint?state=active"))?;
            if let Some(sprint) = sprints
                .get("values")
                .and_then(|v| v.as_array())
                .and_then(|v| v.first())
                .and_then(parse_sprint)
            {
                return Ok(Some(sprint));
            }
        }
        Ok(None)
    }

    /// Moves issues into a sprint.
    pub fn move_to_sprint(&self, sprint: u64, keys: &[&str]) -> Result<()> {
        self.post_empty(
            &format!("/rest/agile/1.0/sprint/{sprint}/issue"),
            serde_json::json!({ "issues": keys }),
        )
    }

    /// The transitions an issue can take from where it is.
    pub fn transitions(&self, key: &str) -> Result<Vec<Transition>> {
        let value = self.get(&format!("/rest/api/3/issue/{}/transitions", key.trim()))?;
        Ok(parse_transitions(&value))
    }

    /// Takes a transition.
    pub fn transition(&self, key: &str, transition_id: &str) -> Result<()> {
        self.post_empty(
            &format!("/rest/api/3/issue/{}/transitions", key.trim()),
            serde_json::json!({ "transition": { "id": transition_id } }),
        )
    }

    /// Moves an issue to In Progress, if its workflow offers a step there.
    /// Returns the step taken, or `None` when there was none.
    pub fn start_progress(&self, key: &str) -> Result<Option<String>> {
        let transitions = self.transitions(key)?;
        let Some(step) = pick_in_progress(&transitions) else { return Ok(None) };
        self.transition(key, &step.id)?;
        Ok(Some(step.name.clone()))
    }

    /// Adds a comment, written in Markdown.
    pub fn add_comment(&self, key: &str, markdown: &str) -> Result<()> {
        self.post(
            &format!("/rest/api/3/issue/{}/comment", key.trim()),
            serde_json::json!({ "body": to_adf(markdown) }),
        )
        .map(drop)
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let resp = agent()
            .get(&format!("{}{path}", self.creds.site))
            .set("Authorization", &self.creds.header())
            .set("Accept", "application/json")
            .call();
        read_json(resp)
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let resp = agent()
            .post(&format!("{}{path}", self.creds.site))
            .set("Authorization", &self.creds.header())
            .set("Accept", "application/json")
            .send_json(body);
        read_json(resp)
    }

    /// A POST whose success is a 204 with no body.
    fn post_empty(&self, path: &str, body: serde_json::Value) -> Result<()> {
        let resp = agent()
            .post(&format!("{}{path}", self.creds.site))
            .set("Authorization", &self.creds.header())
            .set("Accept", "application/json")
            .send_json(body);
        read_empty(resp)
    }

    /// A PUT whose success is a 204 with no body.
    fn put_empty(&self, path: &str, body: serde_json::Value) -> Result<()> {
        let resp = agent()
            .put(&format!("{}{path}", self.creds.site))
            .set("Authorization", &self.creds.header())
            .set("Accept", "application/json")
            .send_json(body);
        read_empty(resp)
    }
}

fn parse_sprint(value: &serde_json::Value) -> Option<Sprint> {
    Some(Sprint { id: value.get("id")?.as_u64()?, name: string_at(value, "name") })
}

fn parse_transitions(value: &serde_json::Value) -> Vec<Transition> {
    value
        .get("transitions")
        .and_then(|t| t.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    Some(Transition {
                        id: t.get("id")?.as_str()?.to_string(),
                        name: string_at(t, "name"),
                        to_category: t
                            .pointer("/to/statusCategory/key")
                            .and_then(|k| k.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The step that means "someone is on it": the first into the in-progress
/// category, else one named like it. Workflows differ; the category is the
/// part Jira keeps consistent.
pub fn pick_in_progress(transitions: &[Transition]) -> Option<&Transition> {
    transitions
        .iter()
        .find(|t| t.to_category == "indeterminate")
        .or_else(|| transitions.iter().find(|t| t.name.to_lowercase().contains("progress")))
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// The query for a project's unassigned, open issues. Bounded by project,
/// which the new search endpoint insists on.
pub fn backlog_jql(project: &str) -> String {
    let key = project.trim().replace('"', "");
    format!(
        "project = \"{key}\" AND assignee is EMPTY AND resolution = Unresolved AND \
         statusCategory != Done ORDER BY priority DESC, created ASC"
    )
}

/// Percent-encodes a query-string value: everything but the unreserved set.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn parse_backlog_issue(value: &serde_json::Value, site: &str) -> Option<BacklogIssue> {
    let key = value.get("key")?.as_str()?.to_string();
    let fields = value.get("fields").cloned().unwrap_or(serde_json::Value::Null);
    let named = |field: &str| {
        fields.get(field).and_then(|v| v.get("name")).and_then(|n| n.as_str()).unwrap_or_default().to_string()
    };
    let description = match fields.get("description") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(doc) if doc.is_object() => adf_to_text(doc),
        _ => String::new(),
    };
    Some(BacklogIssue {
        id: string_at(value, "id"),
        url: format!("{site}/browse/{key}"),
        key,
        summary: string_at(&fields, "summary"),
        description,
        issue_type: named("issuetype"),
        priority: named("priority"),
        status: named("status"),
        labels: fields
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|l| l.iter().filter_map(|v| v.as_str()).map(String::from).collect())
            .unwrap_or_default(),
        updated: string_at(&fields, "updated"),
    })
}

/// Renders an Atlassian Document Format tree as plain text, the inverse of
/// [`to_adf`] as far as a prompt needs: paragraphs and headings become
/// lines, list items get a dash, code blocks keep their fences, and every
/// text node survives whatever it was wrapped in.
pub fn adf_to_text(doc: &serde_json::Value) -> String {
    let mut out = String::new();
    adf_node_text(doc, &mut out, 0);
    // Collapse the blank-line pile-up nesting leaves behind.
    let mut text = String::new();
    let mut blank = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text.trim().to_string()
}

fn adf_node_text(node: &serde_json::Value, out: &mut String, depth: usize) {
    let kind = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let children = node.get("content").and_then(|c| c.as_array());
    match kind {
        "text" => out.push_str(node.get("text").and_then(|t| t.as_str()).unwrap_or("")),
        "hardBreak" => out.push('\n'),
        "mention" | "emoji" | "status" | "date" | "inlineCard" => {
            let attrs = node.get("attrs");
            let label = attrs
                .and_then(|a| a.get("text").or_else(|| a.get("shortName")).or_else(|| a.get("url")))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            out.push_str(label);
        }
        "codeBlock" => {
            let language = node
                .pointer("/attrs/language")
                .and_then(|l| l.as_str())
                .unwrap_or("");
            out.push_str(&format!("```{language}\n"));
            if let Some(children) = children {
                for child in children {
                    adf_node_text(child, out, depth);
                }
            }
            out.push_str("\n```\n\n");
        }
        "listItem" | "taskItem" => {
            out.push_str(&"  ".repeat(depth.saturating_sub(1)));
            out.push_str("- ");
            let mut inner = String::new();
            if let Some(children) = children {
                for child in children {
                    adf_node_text(child, &mut inner, depth + 1);
                }
            }
            out.push_str(inner.trim());
            out.push('\n');
        }
        "paragraph" | "heading" | "blockquote" | "panel" | "tableRow" | "mediaSingle" => {
            if let Some(children) = children {
                for child in children {
                    adf_node_text(child, out, depth);
                }
            }
            out.push('\n');
            if kind != "tableRow" {
                out.push('\n');
            }
        }
        "tableCell" | "tableHeader" => {
            if let Some(children) = children {
                for child in children {
                    let mut cell = String::new();
                    adf_node_text(child, &mut cell, depth);
                    out.push_str(cell.trim());
                }
            }
            out.push_str(" | ");
        }
        "rule" => out.push_str("---\n\n"),
        _ => {
            let is_list = kind.ends_with("List");
            if let Some(children) = children {
                for child in children {
                    adf_node_text(child, out, depth + usize::from(is_list));
                }
            }
            // A list is a block: what follows starts on its own paragraph.
            if is_list && depth == 0 {
                out.push('\n');
            }
        }
    }
}

fn parse_project(value: &serde_json::Value) -> Option<Project> {
    Some(Project {
        id: string_at(value, "id"),
        key: value.get("key")?.as_str()?.to_string(),
        name: string_at(value, "name"),
    })
}

fn parse_issue_type(value: &serde_json::Value) -> Option<IssueType> {
    Some(IssueType {
        id: string_at(value, "id"),
        name: value.get("name")?.as_str()?.to_string(),
        subtask: value.get("subtask").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

/// Reads a response, turning Jira's error shape into a message worth showing.
fn read_json(
    resp: std::result::Result<ureq::Response, ureq::Error>,
) -> Result<serde_json::Value> {
    match resp {
        Ok(r) => r.into_json().map_err(|e| JiraError(e.to_string())),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(JiraError(format!("Jira error {code}: {}", explain(code, &body))))
        }
        Err(e) => Err(JiraError(e.to_string())),
    }
}

/// Reads a response that carries nothing on success.
fn read_empty(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<()> {
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(JiraError(format!("Jira error {code}: {}", explain(code, &body))))
        }
        Err(e) => Err(JiraError(e.to_string())),
    }
}

/// Jira reports what is wrong in `errorMessages` and `errors`, and reports
/// nothing at all for the two mistakes people actually make.
fn explain(code: u16, body: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(messages) = value.get("errorMessages").and_then(|m| m.as_array()) {
            parts.extend(messages.iter().filter_map(|m| m.as_str()).map(String::from));
        }
        if let Some(errors) = value.get("errors").and_then(|e| e.as_object()) {
            for (field, message) in errors {
                if let Some(text) = message.as_str() {
                    parts.push(format!("{field}: {text}"));
                }
            }
        }
    }
    if parts.is_empty() {
        parts.push(match code {
            401 => "check the email address and API token".into(),
            403 => "the account cannot do this in that project".into(),
            404 => "no such project, or the account cannot see it".into(),
            _ => body.chars().take(300).collect::<String>(),
        });
    }
    parts.join("; ")
}

// ---------------------------------------------------------------------------
// Atlassian Document Format
// ---------------------------------------------------------------------------

/// Converts Markdown into an Atlassian Document Format tree.
///
/// The subset a ticket description uses: paragraphs, bullet and ordered
/// lists, headings, fenced code, and inline code and bold. Anything else is
/// carried through as text rather than dropped — a description that lost a
/// line because the renderer did not recognise it is worse than one with a
/// stray asterisk in it.
pub fn to_adf(markdown: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "doc",
        "version": 1,
        "content": adf_blocks(markdown),
    })
}

fn adf_blocks(markdown: &str) -> Vec<serde_json::Value> {
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut list: Vec<serde_json::Value> = Vec::new();
    let mut ordered = false;
    let mut lines = markdown.lines().peekable();

    macro_rules! flush_paragraph {
        () => {
            if !paragraph.is_empty() {
                blocks.push(adf_paragraph(&paragraph.join(" ")));
                paragraph.clear();
            }
        };
    }
    macro_rules! flush_list {
        () => {
            if !list.is_empty() {
                blocks.push(serde_json::json!({
                    "type": if ordered { "orderedList" } else { "bulletList" },
                    "content": std::mem::take(&mut list),
                }));
            }
        };
    }

    while let Some(line) = lines.next() {
        let text = line.trim();

        if text.is_empty() {
            flush_paragraph!();
            flush_list!();
            continue;
        }

        // Fenced code: everything to the closing fence, verbatim.
        if let Some(info) = text.strip_prefix("```") {
            flush_paragraph!();
            flush_list!();
            let mut code: Vec<&str> = Vec::new();
            for next in lines.by_ref() {
                if next.trim_start().starts_with("```") {
                    break;
                }
                code.push(next);
            }
            let language = info.trim();
            let mut node = serde_json::json!({
                "type": "codeBlock",
                "content": [{ "type": "text", "text": code.join("\n") }],
            });
            if !language.is_empty() {
                node["attrs"] = serde_json::json!({ "language": language });
            }
            // ADF rejects a code block with no text content.
            if code.is_empty() {
                node["content"] = serde_json::json!([{ "type": "text", "text": " " }]);
            }
            blocks.push(node);
            continue;
        }

        // Heading.
        let hashes = text.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) {
            if let Some(rest) = text[hashes..].strip_prefix(' ') {
                flush_paragraph!();
                flush_list!();
                blocks.push(serde_json::json!({
                    "type": "heading",
                    "attrs": { "level": hashes },
                    "content": adf_text(rest.trim()),
                }));
                continue;
            }
        }

        // List item.
        if let Some((is_ordered, item)) = list_item(text) {
            flush_paragraph!();
            if !list.is_empty() && is_ordered != ordered {
                flush_list!();
            }
            ordered = is_ordered;
            list.push(serde_json::json!({
                "type": "listItem",
                "content": [adf_paragraph(item)],
            }));
            continue;
        }

        flush_list!();
        paragraph.push(text.to_string());
    }
    flush_paragraph!();
    flush_list!();

    // A document with no content at all is rejected.
    if blocks.is_empty() {
        blocks.push(adf_paragraph(""));
    }
    blocks
}

/// `- item`, `* item`, `1. item`, `2) item`.
fn list_item(text: &str) -> Option<(bool, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            return Some((false, rest.trim()));
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        for sep in [". ", ") "] {
            if let Some(rest) = text[digits..].strip_prefix(sep) {
                return Some((true, rest.trim()));
            }
        }
    }
    None
}

fn adf_paragraph(text: &str) -> serde_json::Value {
    serde_json::json!({ "type": "paragraph", "content": adf_text(text) })
}

/// Inline spans: `**bold**` and `` `code` ``, everything else as plain text.
fn adf_text(text: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let push = |out: &mut Vec<serde_json::Value>, s: &str, mark: Option<&str>| {
        if s.is_empty() {
            return;
        }
        match mark {
            Some(mark) => out.push(serde_json::json!({
                "type": "text", "text": s, "marks": [{ "type": mark }],
            })),
            None => out.push(serde_json::json!({ "type": "text", "text": s })),
        }
    };

    while i < chars.len() {
        // Inline code wins over emphasis, as it does everywhere else.
        if chars[i] == '`' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                if end > i + 1 {
                    push(&mut out, &buf, None);
                    buf.clear();
                    push(&mut out, &chars[i + 1..end].iter().collect::<String>(), Some("code"));
                    i = end + 1;
                    continue;
                }
            }
        }
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = (i + 2..chars.len().saturating_sub(1))
                .find(|&j| chars[j] == '*' && chars[j + 1] == '*')
            {
                if end > i + 2 {
                    push(&mut out, &buf, None);
                    buf.clear();
                    push(
                        &mut out,
                        &chars[i + 2..end].iter().collect::<String>(),
                        Some("strong"),
                    );
                    i = end + 2;
                    continue;
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    push(&mut out, &buf, None);
    // ADF rejects an empty paragraph's content array being absent, but an
    // empty array is fine — a blank paragraph is a blank line.
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_is_recognised_however_it_was_pasted() {
        for input in [
            "acme",
            "acme.atlassian.net",
            "https://acme.atlassian.net",
            "https://acme.atlassian.net/",
            "http://acme.atlassian.net",
            // What someone pastes out of the address bar while looking at a board.
            "https://acme.atlassian.net/jira/software/projects/ABC/boards/1",
            "  acme.atlassian.net  ",
        ] {
            assert_eq!(normalise_site(input), "https://acme.atlassian.net", "{input}");
        }
        // A self-hosted host keeps its own domain rather than being turned
        // into a Cloud subdomain.
        assert_eq!(normalise_site("jira.example.com"), "https://jira.example.com");
        assert_eq!(normalise_site(""), "");
        assert_eq!(normalise_site("   "), "");
    }

    #[test]
    fn an_issues_fields_are_the_shape_jira_wants() {
        let issue = NewIssue {
            project_key: " ABC ".into(),
            issue_type: "Task".into(),
            summary: "  Add a --json flag  ".into(),
            description: "Because scripts need it.".into(),
            labels: vec!["cli".into()],
            parent: None,
        };
        let fields = issue.fields();

        assert_eq!(fields["project"]["key"], "ABC", "the key is trimmed");
        assert_eq!(fields["summary"], "Add a --json flag");
        assert_eq!(fields["issuetype"]["name"], "Task");
        assert_eq!(fields["labels"][0], "cli");
        assert!(fields.get("parent").is_none(), "no parent means no parent field");
        // The description is a document, not a string. Sending a string is a
        // 400 with a message about "Operation value must be an Atlassian
        // Document", which is not a hint anyone enjoys receiving.
        assert_eq!(fields["description"]["type"], "doc");
        assert_eq!(fields["description"]["version"], 1);
    }

    #[test]
    fn a_label_with_a_space_is_hyphenated_rather_than_rejected() {
        // Jira refuses a label containing whitespace, and "needs design" is
        // exactly what a model writes when asked for labels.
        let issue = NewIssue {
            project_key: "ABC".into(),
            issue_type: "Task".into(),
            summary: "s".into(),
            labels: vec!["needs design".into(), "  ".into(), " tech debt ".into()],
            ..Default::default()
        };
        let labels = issue.fields()["labels"].clone();
        assert_eq!(labels[0], "needs-design");
        assert_eq!(labels[1], "tech-debt", "an empty label is dropped, not sent");
        assert_eq!(labels.as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_empty_description_is_left_out_entirely() {
        let issue = NewIssue {
            project_key: "ABC".into(),
            issue_type: "Task".into(),
            summary: "s".into(),
            description: "   \n  ".into(),
            ..Default::default()
        };
        assert!(issue.fields().get("description").is_none());
    }

    /// The block types a ticket description actually uses.
    #[test]
    fn markdown_becomes_a_document() {
        let doc = to_adf(
            "Some prose that\nwraps in the source.\n\n\
             ## Acceptance\n\n\
             - first thing\n- second thing\n\n\
             1. step one\n2. step two\n\n\
             ```rust\nfn main() {}\n```\n",
        );
        assert_eq!(doc["type"], "doc");
        let blocks = doc["content"].as_array().unwrap();
        let kinds: Vec<&str> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            ["paragraph", "heading", "bulletList", "orderedList", "codeBlock"]
        );

        // A wrapped paragraph is one paragraph, not two.
        assert_eq!(blocks[0]["content"][0]["text"], "Some prose that wraps in the source.");
        assert_eq!(blocks[1]["attrs"]["level"], 2);
        assert_eq!(blocks[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(
            blocks[2]["content"][0]["content"][0]["content"][0]["text"],
            "first thing"
        );
        assert_eq!(blocks[4]["attrs"]["language"], "rust");
        assert_eq!(blocks[4]["content"][0]["text"], "fn main() {}");
    }

    #[test]
    fn inline_code_and_bold_become_marks() {
        let doc = to_adf("Run `cargo test` and **do not** skip it.");
        let spans = doc["content"][0]["content"].as_array().unwrap();
        let texts: Vec<&str> = spans.iter().map(|s| s["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["Run ", "cargo test", " and ", "do not", " skip it."]);
        assert_eq!(spans[1]["marks"][0]["type"], "code");
        assert_eq!(spans[3]["marks"][0]["type"], "strong");
        assert!(spans[0].get("marks").is_none(), "plain text carries no marks");
    }

    #[test]
    fn nothing_in_a_description_is_silently_dropped() {
        // Constructs the converter does not model must survive as text. A
        // description that lost a line because of a stray character is worse
        // than one with a stray character in it.
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\nplain";
        let doc = to_adf(source);
        let mut text = String::new();
        collect_text(&doc, &mut text);
        for fragment in ["| a | b |", "quoted", "plain"] {
            assert!(text.contains(fragment), "lost {fragment:?} from:\n{text}");
        }
    }

    #[test]
    fn an_empty_description_still_makes_a_valid_document() {
        // ADF rejects a doc with no content, so this cannot be an empty list.
        let doc = to_adf("");
        assert_eq!(doc["content"].as_array().unwrap().len(), 1);
        assert_eq!(doc["content"][0]["type"], "paragraph");
    }

    #[test]
    fn an_unterminated_fence_does_not_swallow_the_rest() {
        let doc = to_adf("before\n\n```\nunclosed code");
        let kinds: Vec<&str> =
            doc["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["paragraph", "codeBlock"]);
        assert_eq!(doc["content"][1]["content"][0]["text"], "unclosed code");
    }

    fn collect_text(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                    out.push('\n');
                }
                for (_, v) in map {
                    collect_text(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_text(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn a_jira_error_says_what_jira_said() {
        let body = r#"{"errorMessages":["Field 'foo' cannot be set"],
                       "errors":{"summary":"You must specify a summary"}}"#;
        let message = explain(400, body);
        assert!(message.contains("Field 'foo' cannot be set"), "{message}");
        assert!(message.contains("summary: You must specify a summary"), "{message}");

        // And when it says nothing, the code is turned into something
        // actionable rather than shown as a bare number.
        assert!(explain(401, "").contains("API token"));
        assert!(explain(404, "{}").contains("project"));
    }

    #[test]
    fn a_document_becomes_readable_text() {
        let doc = to_adf(
            "Intro line.\n\n## Steps\n\n- first\n- second\n\n```rust\nfn x() {}\n```\n",
        );
        let text = adf_to_text(&doc);
        assert_eq!(text, "Intro line.\n\nSteps\n\n- first\n- second\n\n```rust\nfn x() {}\n```");

        // What Jira actually sends: mentions, hard breaks, a table.
        let doc = serde_json::json!({"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "Ask "},
                {"type": "mention", "attrs": {"id": "1", "text": "@Ana"}},
                {"type": "hardBreak"},
                {"type": "text", "text": "then fix", "marks": [{"type": "strong"}]}]},
            {"type": "table", "content": [{"type": "tableRow", "content": [
                {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
                {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]}]}]},
            {"type": "orderedList", "content": [
                {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "one"}]}]}]}
        ]});
        let text = adf_to_text(&doc);
        assert!(text.starts_with("Ask @Ana\nthen fix"), "{text}");
        assert!(text.contains("a | b |"), "{text}");
        assert!(text.contains("- one"), "{text}");
    }

    #[test]
    fn a_backlog_issue_is_read_from_the_search_shape() {
        let value = serde_json::json!({
            "id": "10001", "key": "ABC-7",
            "fields": {
                "summary": "Crash on empty repo",
                "description": {"type": "doc", "version": 1, "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "It unwraps."}]}]},
                "issuetype": {"name": "Bug"}, "priority": {"name": "High"},
                "status": {"name": "To Do"}, "labels": ["cli", "crash"],
                "updated": "2026-09-01T10:00:00.000+0000"
            }
        });
        let issue = parse_backlog_issue(&value, "https://acme.atlassian.net").unwrap();
        assert_eq!(issue.key, "ABC-7");
        assert_eq!(issue.summary, "Crash on empty repo");
        assert_eq!(issue.description, "It unwraps.");
        assert_eq!(issue.issue_type, "Bug");
        assert_eq!(issue.priority, "High");
        assert_eq!(issue.labels, ["cli", "crash"]);
        assert_eq!(issue.url, "https://acme.atlassian.net/browse/ABC-7");
        let text = issue.prompt_text(1000);
        assert!(text.starts_with("ABC-7: Crash on empty repo (type: Bug; priority: High; labels: cli, crash)\nIt unwraps."), "{text}");
        // A description that is a plain string (older sites) is fine too.
        let value = serde_json::json!({"key": "ABC-8", "fields": {"summary": "s", "description": "plain"}});
        assert_eq!(parse_backlog_issue(&value, "x").unwrap().description, "plain");
        // Truncation lands on a character boundary.
        let long = BacklogIssue { key: "K".into(), summary: "s".into(), description: "é".repeat(50), ..Default::default() };
        assert!(long.prompt_text(21).ends_with("[truncated]"));
    }

    #[test]
    fn the_backlog_query_is_bounded_and_encoded() {
        let jql = backlog_jql(" ABC ");
        assert!(jql.starts_with("project = \"ABC\" AND assignee is EMPTY"), "{jql}");
        assert!(jql.contains("resolution = Unresolved"));
        assert_eq!(percent_encode("a b=\"c\""), "a%20b%3D%22c%22");
        assert_eq!(percent_encode("ABC-7_x.y~"), "ABC-7_x.y~");
    }

    #[test]
    fn the_in_progress_step_is_picked_by_category_then_by_name() {
        let value = serde_json::json!({"transitions": [
            {"id": "11", "name": "Won't Do", "to": {"statusCategory": {"key": "done"}}},
            {"id": "21", "name": "Start work", "to": {"statusCategory": {"key": "indeterminate"}}},
            {"id": "31", "name": "In Progress", "to": {"statusCategory": {"key": "indeterminate"}}}
        ]});
        let transitions = parse_transitions(&value);
        assert_eq!(transitions.len(), 3);
        assert_eq!(pick_in_progress(&transitions).unwrap().id, "21", "the first into the category");

        let by_name = vec![Transition { id: "5".into(), name: "Move to in progress".into(), to_category: String::new() }];
        assert_eq!(pick_in_progress(&by_name).unwrap().id, "5");
        let none = vec![Transition { id: "9".into(), name: "Done".into(), to_category: "done".into() }];
        assert!(pick_in_progress(&none).is_none());

        let sprint = parse_sprint(&serde_json::json!({"id": 42, "name": "Sprint 7", "state": "active"})).unwrap();
        assert_eq!(sprint, Sprint { id: 42, name: "Sprint 7".into() });
    }

    #[test]
    fn credentials_need_all_three_parts() {
        assert!(Credentials::new("acme", "a@b.co", "tok").is_complete());
        assert!(!Credentials::new("", "a@b.co", "tok").is_complete());
        assert!(!Credentials::new("acme", "", "tok").is_complete());
        assert!(!Credentials::new("acme", "a@b.co", "  ").is_complete());
    }
}
