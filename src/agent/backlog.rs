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

const REVIEW_SYSTEM_PROMPT: &str = r#"You are reviewing a change an unattended coding agent made for a Jira ticket, before it becomes a pull request. You have read-only access to the repository with the change applied; the diff is in the message. Read whatever you need to judge it.

Decide:
- Does the change do what the ticket asks — all of it, and only that?
- Is it correct? Look for the bug the ticket describes and check the fix actually removes it; look for edge cases the change ignores; check callers and tests.
- Does it fit the code around it, and leave no debugging output, TODOs, or unrelated edits?
- Is it verified: does the repository's own test or check cover it, and if the ticket is a bug, is there a test that would have caught it?
- Is it written to the standard below? Duplicated logic, a pasted block with variations, a helper the repository already has rewritten, a function doing several things, behaviour kept apart from the data it belongs to, unclear names, magic numbers, dead code: each is a reason to revise, with the file and line.

Be strict. "revise" when anything above is not so, with feedback the agent can act on: what is wrong, where, and what to do. "approve" only when you would merge it.

Answer with JSON only:
{"verdict": "approve" | "revise", "feedback": "…"}"#;

/// The senior engineer an agent turns to when a round did not get
/// through: reads what happened and says what to do next, or that it needs
/// a person.
const ADVISE_SYSTEM_PROMPT: &str = r#"You are the senior engineer pairing with an unattended coding agent working in this repository. Its last attempt did not get through — it changed nothing, or a check failed — and the message says how. You have read-only access to the repository with its attempt applied, if there was one; the diff is in the message.

Work it out before you answer: read the code the task is about, find the failing test or the error's cause, name files and functions. Then decide:
- doable: can the agent finish this without a person deciding anything? false only when it genuinely needs a product or design decision, credentials, or a system the agent does not have. "Hard", "unclear at first glance", or "the agent gave up" are not reasons; make the reasonable assumption and say what it is.
- advice: concrete instructions for the next attempt — what the failure means, which files and functions to change and how, what to run to verify. Write to the standard below, and say so where the attempt did not.

Answer with JSON only:
{"doable": true | false, "advice": "…"}"#;

/// What an advisor said about a failed round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub doable: bool,
    pub advice: String,
}

/// The advisor's task: what was asked, what happened, and the diff so far.
pub fn advise_task(brief: &str, happened: &str, diff: &str) -> String {
    let diff = cap(diff, 60_000);
    let diff_part = if diff.trim().is_empty() {
        "The tree is unchanged: the agent made no edits.".to_string()
    } else {
        format!("The change so far, as a diff against the branch it started from:\n```diff\n{diff}\n```")
    };
    format!(
        "What was asked:\n{}\n\nWhat happened in the last attempt:\n{}\n\n{diff_part}\n\nRead the repository as needed, then answer.",
        cap(brief, MAX_DESCRIPTION),
        cap(happened, 12_000)
    )
}

/// Parses an advisor's reply; a reply that is not JSON is taken as advice
/// to keep going — the opposite default from a verdict, because giving up
/// is the failure this exists to prevent.
pub fn parse_advice(text: &str) -> Advice {
    let json = text.find('{').and_then(|start| text.rfind('}').map(|end| &text[start..=end]));
    if let Some(value) = json.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()) {
        let doable = value.get("doable").and_then(|v| v.as_bool()).unwrap_or(true);
        let advice = value.get("advice").and_then(|f| f.as_str()).unwrap_or("").trim().to_string();
        return Advice { doable, advice };
    }
    Advice { doable: true, advice: text.trim().to_string() }
}

/// Asks the built-in harness, over a read-only workspace on the tree as the
/// agent left it, what to do about a round that did not get through.
pub fn advise(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    brief: &str,
    happened: &str,
    diff: &str,
    on_event: &mut dyn FnMut(Event),
) -> Result<Advice, String> {
    let system = format!("{ADVISE_SYSTEM_PROMPT}\n\n{}", super::coding::CODE_STANDARD);
    let run = super::run(provider, workspace, &system, &advise_task(brief, happened, diff), limits(2), on_event)?;
    Ok(parse_advice(&run.text))
}

/// A reviewer's answer about a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub approve: bool,
    pub feedback: String,
}

/// The reviewer's task: what was asked (a ticket, or a prompt) and the
/// diff, capped.
pub fn review_task(brief: &str, diff: &str) -> String {
    const MAX_DIFF: usize = 60_000;
    let mut diff = diff.to_string();
    if diff.len() > MAX_DIFF {
        let end = (0..=MAX_DIFF).rev().find(|i| diff.is_char_boundary(*i)).unwrap_or(0);
        diff.truncate(end);
        diff.push_str("\n[diff truncated; read the files for the rest]");
    }
    format!(
        "What was asked:\n{}\n\nThe change, as a diff against the branch it started from:\n```diff\n{diff}\n```\n\nRead the repository as needed, then give your verdict.",
        cap(brief, MAX_DESCRIPTION)
    )
}

/// The first `max` bytes of a text, cut at a character boundary.
fn cap(text: &str, max: usize) -> String {
    let mut text = text.trim().to_string();
    if text.len() > max {
        let end = (0..=max).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
    text
}

/// Parses a reviewer's reply; a reply that is not a verdict is a revise
/// with the reply as feedback, so a confused reviewer cannot approve by
/// accident.
pub fn parse_verdict(text: &str) -> Verdict {
    let json = text.find('{').and_then(|start| text.rfind('}').map(|end| &text[start..=end]));
    if let Some(value) = json.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()) {
        let verdict = value.get("verdict").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
        let feedback = value.get("feedback").and_then(|f| f.as_str()).unwrap_or("").trim().to_string();
        return Verdict { approve: verdict == "approve", feedback };
    }
    Verdict { approve: false, feedback: text.trim().to_string() }
}

/// Reviews a change with the built-in harness over a read-only workspace
/// on the tree that has it applied.
pub fn review(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    brief: &str,
    diff: &str,
    on_event: &mut dyn FnMut(Event),
) -> Result<Verdict, String> {
    let system = format!("{REVIEW_SYSTEM_PROMPT}\n\n{}", super::coding::CODE_STANDARD);
    let run = super::run(provider, workspace, &system, &review_task(brief, diff), limits(2), on_event)?;
    Ok(parse_verdict(&run.text))
}

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
    fn a_verdict_is_parsed_and_anything_else_is_a_revise() {
        let v = parse_verdict(r#"Sure. {"verdict": "Approve", "feedback": "clean"}"#);
        assert!(v.approve);
        assert_eq!(v.feedback, "clean");
        let v = parse_verdict(r#"{"verdict": "revise", "feedback": "missing a test"}"#);
        assert!(!v.approve);
        let v = parse_verdict("I could not decide.");
        assert!(!v.approve, "no verdict is not an approval");
        assert_eq!(v.feedback, "I could not decide.");
        let task = review_task(&issue("B-1", "x").prompt_text(1_000), &"+line\n".repeat(100_000));
        assert!(task.contains("[diff truncated"));
        assert!(task.starts_with("What was asked:\nB-1: x"), "{task}");

        // Advice: no JSON means keep going, and "doable" defaults to true.
        let a = parse_advice(r#"{"doable": false, "advice": "needs a product call"}"#);
        assert!(!a.doable);
        let a = parse_advice("Look at lib.py line 2.");
        assert!(a.doable);
        assert_eq!(a.advice, "Look at lib.py line 2.");
        let t = advise_task("fix it", "the check failed:\nE  assert 4 == 3", "");
        assert!(t.contains("The tree is unchanged"), "{t}");
        let t = advise_task("fix it", "changed nothing", "+x");
        assert!(t.contains("```diff\n+x"), "{t}");
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
