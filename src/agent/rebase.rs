//! Proposing a tidier history: which commits fold together, and what the
//! resulting commits should say.
//!
//! The model proposes; [`crate::git::Repo::rewrite_history`] decides whether
//! the proposal is safe to run. Every sha must be accounted for exactly once
//! — a plan that quietly drops a commit is lost work — and that check is
//! deterministic, not something the prompt asks for nicely.
//!
//! A plan that fails the check is handed back to the model once, with the
//! reason. Models miscount shas; being told "you dropped 2 commits" is
//! usually enough.

use super::{Access, Event, Limits, Provider, Workspace};
use crate::git::{Commit, RebaseGroup, RebasePlan};

const SYSTEM_PROMPT: &str = r#"You are tidying a branch's commit history before it is opened as a pull request.

You are given the commits, oldest first, and the diff they add up to. Propose the commits the branch *should* have.

What to do:
- Fold work-in-progress commits into the change they were building. Three commits called "wip", "wip 2" and "fix typo" are one commit.
- Fold a commit that only fixes an earlier commit on this branch into that commit.
- Keep genuinely separate changes separate. A refactor and the feature built on it are two commits, and a reviewer wants them apart.
- Write each resulting commit's message properly: a conventional-commit style summary under 72 characters, imperative mood, and a body explaining *why* where the summary cannot. Take the reasoning from the original commit bodies rather than inventing it.
- Keep the original order unless a later commit must come first to make sense.

Hard rules:
- Every sha you were given must appear exactly once across your groups. Do not drop one, do not repeat one, do not invent one. This is checked, and a plan that fails is rejected.
- Do not propose splitting a commit; you can only fold and reorder.
- If the history is already clean, say so by returning each commit as its own group with its existing message.

Answer with JSON only:
{"commits": [{"use": ["<full sha>", "<full sha>"], "summary": "feat: add the thing",
              "description": "Why, when the summary cannot say it. May be empty."}],
 "notes": "one sentence on what you changed and why, for the developer"}"#;

/// A proposed rewrite, plus what the model says it did.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub plan: RebasePlan,
    pub notes: String,
}

pub fn limits() -> Limits {
    Limits { max_turns: 10, max_tool_calls: 16, max_read_bytes: 150_000, max_tokens: 4096, max_transcript_bytes: 400_000 }
}

/// The task prompt: every commit, oldest first, with its body.
fn task_prompt(commits: &[Commit], diff: &str, max_diff_chars: usize) -> String {
    let mut prompt = format!("{} commit(s) on this branch, oldest first:\n", commits.len());
    for commit in commits.iter().rev() {
        prompt.push_str(&format!("\n{} {}", commit.sha, commit.subject));
        let body = commit.body.trim();
        if !body.is_empty() {
            for line in body.lines() {
                prompt.push_str(&format!("\n    {line}"));
            }
        }
    }
    prompt.push_str("\n\nThe diff they add up to:\n\n```diff\n");
    prompt.push_str(&crate::ollama::truncate_for_prompt(diff, max_diff_chars));
    prompt.push_str("\n```\n\nPropose the commits this branch should have.");
    prompt
}

/// Asks for a plan, and checks it before returning it.
///
/// `branch_commits` is the authority on what may appear in the plan; the
/// model's answer is validated against it and retried once with the reason.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    base: &str,
    commits: &[Commit],
    diff: &str,
    extra_instructions: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Proposal, String> {
    if commits.len() < 2 {
        return Err("There is nothing to tidy: the branch has one commit.".into());
    }
    let system = match extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => SYSTEM_PROMPT.to_string(),
    };
    let known: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();

    let mut task = task_prompt(commits, diff, 40_000);
    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            task = format!(
                "{task}\n\nYour previous plan was rejected: {last_error}\nEvery sha must \
                 appear exactly once. Try again."
            );
        }
        let run = super::run(provider, workspace, &system, &task, limits(), on_event)?;
        let (plan, notes) = match parse(&run.text, base) {
            Some(parsed) => parsed,
            None => {
                last_error = "the answer was not a usable plan".into();
                continue;
            }
        };
        match plan.check_against(&known) {
            Ok(()) => return Ok(Proposal { plan, notes }),
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(format!("The model could not produce a valid plan: {last_error}"))
}

/// Parses `{"commits": [...], "notes": "..."}` into a plan.
fn parse(text: &str, base: &str) -> Option<(RebasePlan, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let items = value.get("commits")?.as_array()?;
    let groups: Vec<RebaseGroup> = items
        .iter()
        .filter_map(|item| {
            let commits: Vec<String> = item
                .get("use")?
                .as_array()?
                .iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect();
            Some(RebaseGroup {
                commits,
                summary: item.get("summary")?.as_str()?.trim().to_string(),
                description: item
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            })
        })
        .collect();
    if groups.is_empty() {
        return None;
    }
    let notes = value
        .get("notes")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Some((RebasePlan { base: base.to_string(), groups }, notes))
}

/// This task only reads.
pub fn access() -> Access {
    Access::ReadOnly
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, Reply, ToolSpec};
    use std::cell::RefCell;

    struct Scripted(RefCell<Vec<String>>);
    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(
            &self,
            _: &str,
            _: &[Message],
            _: &[ToolSpec],
            _: u32,
        ) -> Result<Reply, String> {
            Ok(Reply { text: self.0.borrow_mut().remove(0), calls: vec![], ..Default::default() })
        }
    }

    fn commit(sha: &str, subject: &str) -> Commit {
        Commit {
            sha: sha.to_string(),
            short_sha: sha[..7].to_string(),
            author: "T".into(),
            email: "t@t.io".into(),
            date: "2026-01-01T00:00:00Z".into(),
            subject: subject.into(),
            body: String::new(),
            parents: Vec::new(),
            refs: Vec::new(),
        }
    }

    fn workspace(tmp: &tempfile::TempDir) -> Workspace {
        Workspace::new(tmp.path(), Vec::new(), Access::ReadOnly).unwrap()
    }

    #[test]
    fn a_valid_plan_comes_back_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        // Newest first, as git reports them.
        let commits = vec![commit(&"b".repeat(40), "wip 2"), commit(&"a".repeat(40), "wip")];
        let answer = format!(
            r#"{{"commits": [{{"use": ["{a}", "{b}"], "summary": "feat: the thing",
                 "description": "why"}}], "notes": "folded two wip commits"}}"#,
            a = "a".repeat(40),
            b = "b".repeat(40)
        );
        let provider = Scripted(RefCell::new(vec![answer]));

        let proposal =
            run(&provider, &mut ws, "base", &commits, "a diff", None, &mut |_| {}).unwrap();
        assert_eq!(proposal.plan.groups.len(), 1);
        assert_eq!(proposal.plan.groups[0].summary, "feat: the thing");
        assert_eq!(proposal.plan.groups[0].commits.len(), 2);
        assert_eq!(proposal.notes, "folded two wip commits");
        assert_eq!(proposal.plan.base, "base");
    }

    #[test]
    fn a_plan_that_drops_a_commit_is_retried_once_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let commits = vec![commit(&"b".repeat(40), "two"), commit(&"a".repeat(40), "one")];
        let bad = format!(
            r#"{{"commits": [{{"use": ["{a}"], "summary": "only one"}}]}}"#,
            a = "a".repeat(40)
        );
        let good = format!(
            r#"{{"commits": [{{"use": ["{a}", "{b}"], "summary": "both"}}]}}"#,
            a = "a".repeat(40),
            b = "b".repeat(40)
        );
        let provider = Scripted(RefCell::new(vec![bad, good]));

        let proposal =
            run(&provider, &mut ws, "base", &commits, "d", None, &mut |_| {}).unwrap();
        assert_eq!(proposal.plan.groups[0].commits.len(), 2);
    }

    #[test]
    fn a_plan_that_stays_invalid_is_an_error_not_a_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let commits = vec![commit(&"b".repeat(40), "two"), commit(&"a".repeat(40), "one")];
        let invented = format!(
            r#"{{"commits": [{{"use": ["{a}", "{b}", "{c}"], "summary": "s"}}]}}"#,
            a = "a".repeat(40),
            b = "b".repeat(40),
            c = "c".repeat(40)
        );
        let provider = Scripted(RefCell::new(vec![invented.clone(), invented]));
        let err = run(&provider, &mut ws, "base", &commits, "d", None, &mut |_| {}).unwrap_err();
        assert!(err.contains("not a commit on this branch"), "{err}");
    }

    #[test]
    fn one_commit_needs_no_tidying() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let provider = Scripted(RefCell::new(Vec::new()));
        let err = run(
            &provider,
            &mut ws,
            "base",
            &[commit(&"a".repeat(40), "only")],
            "d",
            None,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("nothing to tidy"), "{err}");
    }

    #[test]
    fn the_prompt_lists_commits_oldest_first() {
        let commits = vec![commit(&"b".repeat(40), "newer"), commit(&"a".repeat(40), "older")];
        let prompt = task_prompt(&commits, "the diff", 1000);
        assert!(prompt.find("older").unwrap() < prompt.find("newer").unwrap());
        assert!(prompt.contains("the diff"));
    }
}
