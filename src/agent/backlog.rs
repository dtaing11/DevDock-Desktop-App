//! Judging a backlog: which unassigned tickets an agent could take.
//!
//! Given the tickets nobody has picked up, the model reads the repository
//! and says, for each one, whether it is about this repository at all,
//! which part of it (a monorepo has several), and whether an agent could do
//! it end to end without a person deciding anything on the way — a bug with
//! a reproduction, a small addition with a clear definition of done — or
//! whether it needs a human first: a design choice, a product decision,
//! access to a system the agent does not have, or a description that does
//! not say what done looks like.
//!
//! Nothing is fixed here. This is the "I could do this one" list; the
//! fixing is [`crate::backlog`], and the user chooses what it gets.

use super::{Event, Limits, Provider, Workspace};
use crate::jira::BacklogIssue;

const SYSTEM_PROMPT: &str = r#"You are triaging a Jira backlog for a coding agent that will work unattended: nobody answers questions while it runs, and its result is a draft pull request a developer reviews afterwards.

You have read-only access to the repository. Use it: search for what a ticket names before deciding whether it is about this code.

For each ticket decide:
- in_scope: is the ticket about code in this repository? Tickets about other services, infrastructure, documentation sites, or things you cannot find any trace of here are not.
- area: the directory or sub-project the work would happen in ("src/cli", "services/billing", or "" when it is the whole repository). A repository may hold several projects; say which.
- autonomous: could an agent finish this without a person deciding anything? Yes when the ticket says what done looks like and the change is mechanical or well-bounded: a bug with a reproduction, a missing check, a small addition with clear behaviour. No when it needs a design or product decision, visual judgement, credentials or a system the agent does not have, a discussion with the reporter, or when the description does not say what the result should be.
- confidence: 0-100, how sure you are of that judgement.
- reason: one sentence a developer can check.
- plan: for an autonomous ticket, two or three lines saying what the agent would change, naming files you found. Empty otherwise.

Judge every ticket you are given; skipping one is an error. Do not fix anything.

Answer with JSON only:
{"tickets": [{"key": "ABC-7", "in_scope": true, "area": "src/cli", "autonomous": true,
              "confidence": 80, "reason": "…", "plan": "…"}]}"#;

/// What the model concluded about one ticket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Triage {
    pub key: String,
    pub in_scope: bool,
    /// Directory or sub-project, empty for the whole repository.
    pub area: String,
    pub autonomous: bool,
    pub confidence: u8,
    pub reason: String,
    pub plan: String,
}

impl Triage {
    /// A ticket worth offering: in this repository, doable unattended, and
    /// the model is at least reasonably sure.
    pub fn suggested(&self) -> bool {
        self.in_scope && self.autonomous && self.confidence >= 50
    }
}

/// How much of one description goes into the prompt.
const MAX_DESCRIPTION: usize = 3_000;

/// The budget for judging `count` tickets: enough to search for each one.
pub fn limits(count: usize) -> Limits {
    Limits {
        max_turns: (6 + 2 * count).clamp(10, 40),
        max_tool_calls: (6 * count).clamp(20, 120),
        max_read_bytes: 400_000,
        max_tokens: 8192,
        max_transcript_bytes: 500_000,
    }
}

/// Judges every ticket, retrying once with the reason when the answer left
/// one out or was not JSON.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    issues: &[BacklogIssue],
    on_event: &mut dyn FnMut(Event),
) -> Result<Vec<Triage>, String> {
    if issues.is_empty() {
        return Err("There are no tickets to judge.".into());
    }
    let listed: String = issues
        .iter()
        .map(|i| format!("---\n{}", i.prompt_text(MAX_DESCRIPTION)))
        .collect::<Vec<_>>()
        .join("\n");
    let overview = workspace.overview();
    let mut task = format!(
        "Repository: {overview}\n\n{} ticket(s) to judge:\n{listed}\n---\n\nRead the \
         repository as needed, then answer for every ticket.",
        issues.len()
    );
    let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();

    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            task = format!(
                "{task}\n\nYour previous answer was rejected: {last_error}\nAnswer for \
                 every ticket and try again."
            );
        }
        let run = super::run(provider, workspace, SYSTEM_PROMPT, &task, limits(issues.len()), on_event)?;
        let Some(triage) = parse(&run.text) else {
            last_error = "the answer was not a usable list of judgements".into();
            continue;
        };
        match check(&triage, &keys) {
            Ok(()) => return Ok(order_like(triage, &keys)),
            Err(e) => last_error = e,
        }
    }
    Err(format!("The model could not judge the backlog: {last_error}"))
}

/// Every key judged exactly once, and no key invented.
fn check(triage: &[Triage], keys: &[&str]) -> Result<(), String> {
    for t in triage {
        if !keys.contains(&t.key.as_str()) {
            return Err(format!("{} is not one of the tickets", t.key));
        }
    }
    let missing: Vec<&str> =
        keys.iter().copied().filter(|k| !triage.iter().any(|t| t.key == *k)).collect();
    if !missing.is_empty() {
        return Err(format!("no judgement for {}", missing.join(", ")));
    }
    Ok(())
}

/// The judgements in the order the tickets were given, first judgement per
/// key winning.
fn order_like(triage: Vec<Triage>, keys: &[&str]) -> Vec<Triage> {
    keys.iter()
        .filter_map(|k| triage.iter().find(|t| t.key == *k).cloned())
        .collect()
}

fn parse(text: &str) -> Option<Vec<Triage>> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let tickets = value.get("tickets")?.as_array()?;
    let string = |v: &serde_json::Value, key: &str| {
        v.get(key).and_then(|x| x.as_str()).unwrap_or_default().trim().to_string()
    };
    let flag = |v: &serde_json::Value, key: &str| v.get(key).and_then(|x| x.as_bool()).unwrap_or(false);
    Some(
        tickets
            .iter()
            .filter_map(|t| {
                let key = string(t, "key");
                (!key.is_empty()).then(|| Triage {
                    key,
                    in_scope: flag(t, "in_scope"),
                    area: string(t, "area").trim_matches('/').to_string(),
                    autonomous: flag(t, "autonomous"),
                    confidence: t.get("confidence").and_then(|c| c.as_u64()).unwrap_or(0).min(100) as u8,
                    reason: string(t, "reason"),
                    plan: string(t, "plan"),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Access, Message, Reply, ToolSpec};
    use std::cell::RefCell;

    struct Scripted(RefCell<Vec<Reply>>);
    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(&self, system: &str, _: &[Message], _: &[ToolSpec], _: u32) -> Result<Reply, String> {
            assert!(system.contains("triaging"));
            Ok(self.0.borrow_mut().remove(0))
        }
    }

    fn issue(key: &str, summary: &str) -> BacklogIssue {
        BacklogIssue { key: key.into(), summary: summary.into(), ..Default::default() }
    }

    fn workspace(tmp: &tempfile::TempDir) -> Workspace {
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        Workspace::new(tmp.path(), vec!["a.rs".into()], Access::ReadOnly).unwrap()
    }

    #[test]
    fn a_reply_becomes_judgements_in_ticket_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let reply = r#"{"tickets": [
            {"key": "B-2", "in_scope": false, "area": "", "autonomous": false, "confidence": 90, "reason": "about the website", "plan": ""},
            {"key": "B-1", "in_scope": true, "area": "/src/", "autonomous": true, "confidence": 75, "reason": "unwrap in a.rs", "plan": "guard it"}
        ]}"#;
        let provider = Scripted(RefCell::new(vec![Reply { text: reply.into(), ..Default::default() }]));
        let out = run(&provider, &mut ws, &[issue("B-1", "crash"), issue("B-2", "site")], &mut |_| {}).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, "B-1", "the order the tickets were given");
        assert!(out[0].suggested());
        assert_eq!(out[0].area, "src", "slashes are trimmed");
        assert!(!out[1].suggested());
    }

    #[test]
    fn a_missing_or_invented_ticket_is_rejected_then_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let bad = r#"{"tickets": [{"key": "B-1", "in_scope": true, "autonomous": true, "confidence": 60}]}"#;
        let good = r#"{"tickets": [{"key": "B-1", "in_scope": true, "autonomous": true, "confidence": 60},
                                    {"key": "B-2", "in_scope": false, "confidence": 60}]}"#;
        let provider = Scripted(RefCell::new(vec![
            Reply { text: bad.into(), ..Default::default() },
            Reply { text: good.into(), ..Default::default() },
        ]));
        let out = run(&provider, &mut ws, &[issue("B-1", "x"), issue("B-2", "y")], &mut |_| {}).unwrap();
        assert_eq!(out.len(), 2);

        assert!(check(&[Triage { key: "Z-9".into(), in_scope: true, area: String::new(), autonomous: true, confidence: 1, reason: String::new(), plan: String::new() }], &["B-1"]).is_err());
    }

    #[test]
    fn low_confidence_is_not_a_suggestion() {
        let t = Triage { key: "k".into(), in_scope: true, area: String::new(), autonomous: true, confidence: 40, reason: String::new(), plan: String::new() };
        assert!(!t.suggested());
        assert!(Triage { confidence: 50, ..t }.suggested());
    }

    #[test]
    fn an_empty_backlog_is_refused_before_a_model_is_called() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let provider = Scripted(RefCell::new(vec![]));
        assert!(run(&provider, &mut ws, &[], &mut |_| {}).is_err());
    }
}
