//! End-to-end tests of the AI harness against throwaway repositories.
//!
//! The provider is scripted rather than a real model: what is under test is
//! the harness contract — what the tools can reach, that proposals stay off
//! disk until applied, and that an applied proposal actually resolves a
//! merge — not a model's judgement.

use git_manage::agent::{
    self, conflict, Access, Limits, Message, Provider, Reply, ToolCall, ToolSpec, Workspace,
};
use git_manage::git::{Repo, RepoState, Resolution};
use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository mid-merge, with `conflict.txt` conflicted, one other tracked
/// file, and one ignored file that must stay invisible.
fn conflicted_repo() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sh(dir, &["init", "-b", "main"]);
    sh(dir, &["config", "user.email", "test@test.io"]);
    sh(dir, &["config", "user.name", "Tester"]);

    fs::write(dir.join(".gitignore"), ".env\n").unwrap();
    fs::write(dir.join("conflict.txt"), "base\n").unwrap();
    fs::write(dir.join("caller.txt"), "calls base\n").unwrap();
    fs::write(dir.join(".env"), "SECRET=hunter2\n").unwrap();
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-m", "init"]);

    sh(dir, &["checkout", "-b", "clash"]);
    fs::write(dir.join("conflict.txt"), "theirs\n").unwrap();
    sh(dir, &["commit", "-am", "theirs"]);
    sh(dir, &["checkout", "main"]);
    fs::write(dir.join("conflict.txt"), "ours\n").unwrap();
    sh(dir, &["commit", "-am", "ours"]);

    let repo = Repo::open(dir).unwrap();
    let outcome = repo.merge("clash");
    assert!(outcome.conflict, "expected a conflict: {}", outcome.message);
    (tmp, repo)
}

fn workspace(repo: &Repo, access: Access) -> Workspace {
    Workspace::new(repo.path(), repo.tracked_files().unwrap(), access).unwrap()
}

/// Replays scripted replies and records the transcript it was sent.
struct Scripted {
    replies: RefCell<Vec<Reply>>,
    seen: RefCell<Vec<String>>,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Self {
        Self { replies: RefCell::new(replies), seen: RefCell::new(Vec::new()) }
    }

    /// Every tool result the harness fed back, in order.
    fn tool_results(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }
}

impl Provider for Scripted {
    fn label(&self) -> String {
        "scripted".into()
    }

    fn turn(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
        _max_tokens: u32,
    ) -> Result<Reply, String> {
        if let Some(Message::ToolResults(results)) = messages.last() {
            self.seen.borrow_mut().extend(results.iter().map(|r| r.content.clone()));
        }
        let mut replies = self.replies.borrow_mut();
        assert!(!replies.is_empty(), "the harness asked for more turns than were scripted");
        Ok(replies.remove(0))
    }
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), input }
}

fn calls(text: &str, calls: Vec<ToolCall>) -> Reply {
    Reply { text: text.into(), calls }
}

#[test]
fn the_conflict_harness_proposes_changes_that_only_land_when_applied() {
    let (_tmp, repo) = conflicted_repo();
    let files: Vec<conflict::Brief> = repo
        .conflicts()
        .unwrap()
        .into_iter()
        .map(|f| conflict::Brief { path: f.path, ours: f.ours, theirs: f.theirs })
        .collect();
    assert_eq!(files.len(), 1);

    let provider = Scripted::new(vec![
        // Look at the conflicted file, and at a file the merge affects.
        calls(
            "reading",
            vec![
                call("1", "read_file", serde_json::json!({"path": "conflict.txt"})),
                call("2", "search", serde_json::json!({"query": "base"})),
            ],
        ),
        // Propose a merge, plus a change outside the conflicted file.
        calls(
            "",
            vec![
                call(
                    "3",
                    "write_file",
                    serde_json::json!({"path": "conflict.txt", "content": "ours\ntheirs\n"}),
                ),
                call(
                    "4",
                    "edit_file",
                    serde_json::json!({
                        "path": "caller.txt",
                        "old_text": "calls base",
                        "new_text": "calls ours and theirs"
                    }),
                ),
            ],
        ),
        calls("Kept both sides; updated the caller.", vec![]),
    ]);

    let mut ws = workspace(&repo, Access::ReadWrite);
    let run = conflict::run(
        &provider,
        &mut ws,
        &files,
        None,
        conflict::limits(),
        &mut |_| {},
    )
    .unwrap();

    // The model saw the working copy, markers and all.
    let results = provider.tool_results();
    assert!(results[0].contains("<<<<<<<"), "read did not show markers: {}", results[0]);
    assert!(results[0].contains("ours") && results[0].contains("theirs"));

    // Two proposals, and the worktree is untouched.
    assert_eq!(run.edits.len(), 2);
    assert_eq!(fs::read_to_string(repo.path().join("caller.txt")).unwrap(), "calls base\n");
    assert!(fs::read_to_string(repo.path().join("conflict.txt")).unwrap().contains("<<<<<<<"));

    // Applying is what writes: the conflicted file through the resolver, the
    // other file straight into the worktree.
    let merged = run.edits.iter().find(|e| e.path == "conflict.txt").unwrap();
    repo.resolve("conflict.txt", &Resolution::Manual(merged.after.clone())).unwrap();
    let caller = run.edits.iter().find(|e| e.path == "caller.txt").unwrap();
    fs::write(repo.path().join("caller.txt"), &caller.after).unwrap();

    assert!(repo.merge_continue().ok);
    assert_eq!(repo.state().unwrap(), RepoState::Clean);
    assert_eq!(fs::read_to_string(repo.path().join("conflict.txt")).unwrap(), "ours\ntheirs\n");
    assert_eq!(
        fs::read_to_string(repo.path().join("caller.txt")).unwrap(),
        "calls ours and theirs\n"
    );
}

#[test]
fn ignored_files_are_unreachable_from_a_real_repository() {
    let (_tmp, repo) = conflicted_repo();
    assert!(!repo.tracked_files().unwrap().contains(&".env".to_string()));

    let provider = Scripted::new(vec![
        calls(
            "",
            vec![
                call("1", "read_file", serde_json::json!({"path": ".env"})),
                call("2", "read_file", serde_json::json!({"path": "../outside"})),
                call("3", "list_files", serde_json::json!({})),
            ],
        ),
        calls("done", vec![]),
    ]);
    let mut ws = workspace(&repo, Access::ReadOnly);
    agent::run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |_| {}).unwrap();

    let results = provider.tool_results();
    assert!(!results.iter().any(|r| r.contains("hunter2")), "{results:?}");
    assert!(results[0].contains("not a tracked file"));
    assert!(results[1].contains("outside the repository"));
    assert!(results[2].contains("conflict.txt") && !results[2].contains(".env"));
}

#[test]
fn a_review_reads_the_repository_and_reports_what_it_read() {
    let (_tmp, repo) = conflicted_repo();
    // Finish the merge so there is a clean tree to review a diff against.
    repo.resolve("conflict.txt", &Resolution::Ours).unwrap();
    assert!(repo.merge_continue().ok);

    let findings = r#"{"summary": "one problem",
      "reasoning": "read caller.txt to check the contract",
      "findings": [{"file": "conflict.txt", "line": 1, "severity": "high",
                    "title": "drops the incoming change",
                    "detail": "caller.txt still expects the other side"}]}"#;
    let provider = Scripted::new(vec![
        calls("", vec![call("1", "read_file", serde_json::json!({"path": "caller.txt"}))]),
        calls(findings, vec![]),
    ]);

    let mut ws = workspace(&repo, Access::ReadOnly);
    let config = git_manage::review::ReviewConfig::default();
    let outcome = git_manage::review::run_with_context(
        &provider,
        &mut ws,
        "diff --git a/conflict.txt b/conflict.txt",
        &config,
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].severity, git_manage::review::Severity::High);
    assert!(outcome.blocking(config.fail_on).len() == 1);
    assert!(outcome.should_block(&config));
    // The reading list travels with the verdict.
    assert!(
        outcome.context_log.iter().any(|l| l.contains("read caller.txt")),
        "{:?}",
        outcome.context_log
    );
}

#[test]
fn a_reviewer_cannot_edit_the_repository() {
    let (_tmp, repo) = conflicted_repo();
    let provider = Scripted::new(vec![
        calls(
            "",
            vec![call(
                "1",
                "write_file",
                serde_json::json!({"path": "conflict.txt", "content": "rewritten\n"}),
            )],
        ),
        calls(r#"{"summary": "ok", "reasoning": "", "findings": []}"#, vec![]),
    ]);
    let mut ws = workspace(&repo, Access::ReadOnly);
    let outcome = git_manage::review::run_with_context(
        &provider,
        &mut ws,
        "a diff",
        &git_manage::review::ReviewConfig::default(),
        &mut |_| {},
    )
    .unwrap();

    assert!(outcome.findings.is_empty());
    assert!(provider.tool_results()[0].contains("read-only"));
    assert!(fs::read_to_string(repo.path().join("conflict.txt")).unwrap().contains("<<<<<<<"));
}
