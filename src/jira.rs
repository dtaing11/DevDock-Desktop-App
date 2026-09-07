//! Jira Cloud: credentials, projects, and creating issues.
//!
//! Enough of the REST API to turn a list of work into tickets: check who you
//! are, list the projects you can file into, list a project's issue types,
//! and create issues. Reading and searching issues is deliberately absent —
//! this exists to put work *into* Jira, and a client that also tried to be a
//! Jira browser would be a worse version of the one in the browser.
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
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
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
    fn credentials_need_all_three_parts() {
        assert!(Credentials::new("acme", "a@b.co", "tok").is_complete());
        assert!(!Credentials::new("", "a@b.co", "tok").is_complete());
        assert!(!Credentials::new("acme", "", "tok").is_complete());
        assert!(!Credentials::new("acme", "a@b.co", "  ").is_complete());
    }
}
