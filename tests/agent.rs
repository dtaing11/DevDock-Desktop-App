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
                    "detail": "caller.txt still expects the other side",
                    "evidence": "ours"}]}"#;
    let provider = Scripted::new(vec![
        calls("", vec![call("1", "read_file", serde_json::json!({"path": "caller.txt"}))]),
        calls(findings, vec![]),
        // Findings are verified before they are shown; this one holds up.
        calls(r#"{"verdicts": [{"index": 0, "keep": true, "why": "confirmed"}]}"#, vec![]),
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

// ---------------------------------------------------------------------------
// Review verification
// ---------------------------------------------------------------------------

/// The reviewer's findings are checked against the code before anyone sees
/// them: a misquoted citation is dropped mechanically, and what survives is
/// judged again with the repository open.
#[test]
fn findings_that_do_not_survive_verification_are_dropped() {
    let (_tmp, repo) = conflicted_repo();
    // Finish the merge so there is a clean tree with a real file to cite.
    repo.resolve("conflict.txt", &Resolution::Ours).unwrap();
    assert!(repo.merge_continue().ok);

    let findings = r#"{"summary": "three findings", "reasoning": "read caller.txt",
      "findings": [
        {"file": "caller.txt", "line": 1, "severity": "high",
         "title": "real problem", "detail": "the call is wrong",
         "evidence": "calls base"},
        {"file": "caller.txt", "line": 1, "severity": "high",
         "title": "misquoted problem", "detail": "this line is not there",
         "evidence": "let never_written_in_this_file = 1;"},
        {"file": "no/such/file.rs", "line": 9, "severity": "medium",
         "title": "wrong file", "detail": "cites a file that does not exist",
         "evidence": "anything"},
        {"file": "caller.txt", "line": 999, "severity": "low",
         "title": "line past the end", "detail": "the citation is out of range",
         "evidence": "calls base"}
      ]}"#;

    // The verifier drops the first finding and keeps the survivor of the
    // citation check.
    let verdicts = r#"{"verdicts": [
        {"index": 0, "keep": false, "why": "caller.txt documents this as intended"},
        {"index": 1, "keep": true, "why": "confirmed"}
    ]}"#;

    let provider = Scripted::new(vec![
        calls(findings, vec![]),
        calls(verdicts, vec![]),
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

    let titles: Vec<&str> = outcome.findings.iter().map(|f| f.title.as_str()).collect();
    // Two dropped mechanically (bad quote, missing file), one dropped by the
    // verifier, one kept.
    assert_eq!(titles, vec!["line past the end"], "{:?}", outcome.context_log);

    let log = outcome.context_log.join("\n");
    assert!(log.contains("misquoted problem"), "{log}");
    assert!(log.contains("wrong file"), "{log}");
    assert!(log.contains("caller.txt documents this as intended"), "{log}");
    assert!(log.contains("did not survive verification"), "{log}");

    // The out-of-range line number is repaired rather than shown as-is.
    assert_ne!(outcome.findings[0].line, Some(999));
}

#[test]
fn verification_that_cannot_run_keeps_the_findings() {
    let (_tmp, repo) = conflicted_repo();
    repo.resolve("conflict.txt", &Resolution::Ours).unwrap();
    assert!(repo.merge_continue().ok);

    let findings = r#"{"summary": "one", "reasoning": "",
      "findings": [{"file": "caller.txt", "line": 1, "severity": "high",
                    "title": "kept", "detail": "d", "evidence": "calls base"}]}"#;
    // The verifier answers with something unparseable.
    let provider = Scripted::new(vec![
        calls(findings, vec![]),
        calls("I could not check these.", vec![]),
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

    assert_eq!(outcome.findings.len(), 1, "a failed check must not discard findings");
    assert!(outcome.context_log.join("\n").contains("findings kept"));
}

#[test]
fn verification_is_skipped_when_it_is_turned_off_or_there_is_nothing_to_check() {
    let (_tmp, repo) = conflicted_repo();
    repo.resolve("conflict.txt", &Resolution::Ours).unwrap();
    assert!(repo.merge_continue().ok);

    // A clean review never pays for a second request.
    let provider = Scripted::new(vec![calls(
        r#"{"summary": "clean", "reasoning": "", "findings": []}"#,
        vec![],
    )]);
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

    // And the switch is honoured.
    let findings = r#"{"summary": "one", "reasoning": "",
      "findings": [{"file": "caller.txt", "line": 1, "severity": "high",
                    "title": "unchecked", "detail": "d", "evidence": "calls base"}]}"#;
    let provider = Scripted::new(vec![calls(findings, vec![])]);
    let config = git_manage::review::ReviewConfig {
        verify_findings: false,
        ..Default::default()
    };
    let mut ws = workspace(&repo, Access::ReadOnly);
    let outcome =
        git_manage::review::run_with_context(&provider, &mut ws, "a diff", &config, &mut |_| {})
            .unwrap();
    assert_eq!(outcome.findings.len(), 1);
}

/// The verifier against the findings that started this: three real false
/// positives from a review of this repository, plus one real defect.
///
/// It has to drop the three and keep the one. Ignored by default — needs
/// Claude credentials and costs tokens.
/// `cargo test --test agent -- --ignored --nocapture live_verifier`
#[test]
#[ignore]
fn live_verifier_drops_the_false_positives() {
    use git_manage::review::{Finding, ReviewConfig, ReviewOutcome, Severity};

    let Some(client) = git_manage::claude::Client::from_store("claude-opus-5") else {
        eprintln!("Claude is not signed in; skipping");
        return;
    };
    let repo = Repo::open(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadOnly)
        .unwrap();

    let finding = |file: &str, line: u32, title: &str, detail: &str, evidence: &str| Finding {
        file: file.into(),
        line: Some(line),
        severity: Severity::High,
        title: title.into(),
        detail: detail.into(),
        evidence: evidence.into(),
    };

    let mut outcome = ReviewOutcome {
        summary: "four findings".into(),
        findings: vec![
            // 1. False: there is nothing to validate client-side.
            finding(
                "src/claude.rs",
                558,
                "OAuth token passed directly in Authorization header without validation",
                "The access token is interpolated into the header with no validation, \
                 which could send a malformed or expired credential.",
                ".set(\"Authorization\", &format!(\"Bearer {}\", tokens.access_token))",
            ),
            // 2. False: the built-in prompt always applies.
            finding(
                "src/app/mod.rs",
                1,
                "Conflict prompt source is unchecked; missing prompt means no guidance",
                "conflict_prompt() returns Option<String> and a None means the model is \
                 given no instructions at all.",
                "let custom = self.conflict_prompt();",
            ),
            // 3. False: serde default plus a Default impl.
            finding(
                "src/review.rs",
                1,
                "repo_context may not be initialized from config",
                "The reviewer depends on cfg.repo_context, which may be uninitialised when \
                 the config file omits it.",
                "pub repo_context: bool,",
            ),
            // 4. Real: this one has to survive.
            finding(
                "src/agent/workspace.rs",
                1,
                "search reads every tracked file on each call",
                "Each search opens every tracked file in turn, so a search in a large \
                 repository reads the whole tree before returning.",
                "for rel in self.visible_paths() {",
            ),
        ],
        ..Default::default()
    };

    let config = ReviewConfig::default();
    git_manage::review::verify(&client, &mut ws, &mut outcome, &config, &mut |e| {
        println!("  {}", e.line())
    });

    println!("\n--- SURVIVED ---");
    for f in &outcome.findings {
        println!("  [{}] {}", f.severity.label(), f.title);
    }
    println!("--- LOG ---");
    for line in &outcome.context_log {
        println!("  {line}");
    }

    let kept: Vec<&str> = outcome.findings.iter().map(|f| f.title.as_str()).collect();
    for false_positive in [
        "OAuth token passed directly in Authorization header without validation",
        "Conflict prompt source is unchecked; missing prompt means no guidance",
        "repo_context may not be initialized from config",
    ] {
        assert!(!kept.contains(&false_positive), "kept a false positive: {false_positive}");
    }
    assert!(
        kept.contains(&"search reads every tracked file on each call"),
        "the real finding was dropped: {kept:?}"
    );
}
