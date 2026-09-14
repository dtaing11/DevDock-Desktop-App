//! End-to-end tests of the AI harness against throwaway repositories.
//!
//! The provider is scripted rather than a real model: what is under test is
//! the harness contract — what the tools can reach, that proposals stay off
//! disk until applied, and that an applied proposal actually resolves a
//! merge — not a model's judgement.

use git_manage::agent::{
    self, coding, conflict, Access, Limits, Message, Provider, Reply, ToolCall, ToolSpec,
    Workspace, WriteMode,
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
    Reply { text: text.into(), calls, ..Default::default() }
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
// The coding agent
// ---------------------------------------------------------------------------

/// A repository the coding agent can actually work in: a source file, a
/// language server it can consult, and a check it can run.
fn coding_repo() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sh(dir, &["init", "-b", "main"]);
    sh(dir, &["config", "user.email", "t@t.io"]);
    sh(dir, &["config", "user.name", "T"]);

    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_lsp.py");
    fs::write(
        dir.join(".git-manage-ci.toml"),
        format!(
            "[[job]]\nname = \"tests\"\ncommands = [\"echo the-suite-ran\"]\n\n\
             [[lsp]]\nextensions = [\"rs\"]\ncommand = \"python3\"\n\
             args = [\"{}\"]\nlanguage_id = \"rust\"\n",
            fake.display()
        ),
    )
    .unwrap();
    fs::write(dir.join("lib.rs"), "pub fn halve(n: u32) -> u32 {\n    n / 2\n}\n").unwrap();
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-m", "init"]);

    let repo = Repo::open(dir).unwrap();
    (tmp, repo)
}

#[test]
fn the_coding_agent_edits_consults_the_server_and_runs_a_check() {
    let (_tmp, repo) = coding_repo();
    let lsp = std::sync::Arc::new(git_manage::lsp::Manager::new(repo.path(), None));
    let jobs = git_manage::local_ci::discover_configs(repo.path()).unwrap().config.jobs;
    assert_eq!(jobs.len(), 1);

    let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
        .unwrap()
        .with_write_mode(WriteMode::Live)
        .with_language_support(lsp)
        .with_checks(jobs);

    // Every tool the agent should have in a fully equipped run.
    let names: Vec<&str> = ws.tools().iter().map(|t| t.name).collect();
    for expected in ["read_file", "edit_file", "diagnostics", "references", "run_check"] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }

    let provider = Scripted::new(vec![
        calls("", vec![call("1", "read_file", serde_json::json!({"path": "lib.rs"}))]),
        calls(
            "",
            vec![call(
                "2",
                "edit_file",
                serde_json::json!({"path": "lib.rs", "old_text": "n / 2", "new_text": "n / 2 + 0"}),
            )],
        ),
        calls("", vec![call("3", "diagnostics", serde_json::json!({"path": "lib.rs"}))]),
        calls(
            "",
            vec![call(
                "4",
                "references",
                serde_json::json!({"path": "lib.rs", "line": 1, "symbol": "halve"}),
            )],
        ),
        calls("", vec![call("5", "run_check", serde_json::json!({"name": "tests"}))]),
        calls("- lib.rs: adjusted halve()", vec![]),
    ]);

    let run = coding::run(
        &provider,
        &mut ws,
        coding::Request { branch: Some("main"), ..coding::Request::new("tweak halve") },
        &mut |_| {},
    )
    .unwrap();

    let results = provider.tool_results();
    // The edit went to disk, because that is what "let it iterate" means.
    assert!(results[1].contains("on disk"), "{}", results[1]);
    assert_eq!(
        fs::read_to_string(repo.path().join("lib.rs")).unwrap(),
        "pub fn halve(n: u32) -> u32 {\n    n / 2 + 0\n}\n"
    );
    // The language server was consulted and answered.
    assert!(results[2].contains("something is wrong"), "{}", results[2]);
    assert!(results[3].contains("lib.rs:"), "{}", results[3]);
    // The project's own check ran, and its output came back.
    assert!(results[4].contains("PASSED"), "{}", results[4]);
    assert!(results[4].contains("the-suite-ran"), "{}", results[4]);

    assert_eq!(run.edits.len(), 1);
    assert_eq!(run.edits[0].path, "lib.rs");
    assert!(run.text.contains("adjusted halve"));

    // And the whole run can be undone exactly.
    ws.revert_all().unwrap();
    assert_eq!(
        fs::read_to_string(repo.path().join("lib.rs")).unwrap(),
        "pub fn halve(n: u32) -> u32 {\n    n / 2\n}\n"
    );
}

#[test]
fn a_proposing_run_cannot_run_checks_and_says_why() {
    let (_tmp, repo) = coding_repo();
    let jobs = git_manage::local_ci::discover_configs(repo.path()).unwrap().config.jobs;
    let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
        .unwrap()
        .with_checks(jobs);

    let provider = Scripted::new(vec![
        calls(
            "",
            vec![call(
                "1",
                "edit_file",
                serde_json::json!({"path": "lib.rs", "old_text": "n / 2", "new_text": "n / 3"}),
            )],
        ),
        calls("", vec![call("2", "run_check", serde_json::json!({"name": "tests"}))]),
        calls("- could not verify", vec![]),
    ]);

    let run = coding::run(&provider, &mut ws, coding::Request::new("change it"), &mut |_| {})
        .unwrap();

    let results = provider.tool_results();
    assert!(results[0].contains("Nothing is on disk yet"), "{}", results[0]);
    assert!(results[1].contains("would test the old code"), "{}", results[1]);
    // Nothing was written, so the check refusing was the right call.
    assert_eq!(
        fs::read_to_string(repo.path().join("lib.rs")).unwrap(),
        "pub fn halve(n: u32) -> u32 {\n    n / 2\n}\n"
    );
    assert_eq!(run.edits.len(), 1);
}

/// The coding agent against a real model, a real language server, and a
/// real build — the whole loop, on a repository with an actual bug in it.
///
/// Ignored by default: it needs Claude credentials, clangd, and a C
/// compiler, and it costs tokens. Run it with
/// `cargo test --test agent -- --ignored --nocapture live_coding_agent`.
#[test]
#[ignore]
fn live_coding_agent() {
    if !git_manage::lsp::registry::on_path("clangd") {
        eprintln!("clangd not installed; skipping");
        return;
    }
    let Some(client) = git_manage::claude::Client::from_store("claude-opus-5") else {
        eprintln!("Claude is not signed in; skipping");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sh(dir, &["init", "-b", "main"]);
    sh(dir, &["config", "user.email", "t@t.io"]);
    sh(dir, &["config", "user.name", "T"]);
    fs::write(
        dir.join(".git-manage-ci.toml"),
        "[[job]]\nname = \"build\"\ncommands = [\"cc -Wall -Werror -c main.c -o /dev/null\"]\n",
    )
    .unwrap();
    // total() is declared to return int but falls off the end, and main
    // passes the wrong type. Both are only visible if you actually build it.
    fs::write(
        dir.join("main.c"),
        "#include <stdio.h>\n\n\
         int total(int *values, int count) {\n\
         \x20   int sum = 0;\n\
         \x20   for (int i = 0; i <= count; i++) {\n\
         \x20       sum += values[i];\n\
         \x20   }\n\
         }\n\n\
         int main(void) {\n\
         \x20   int values[3] = {1, 2, 3};\n\
         \x20   printf(\"%d\\n\", total(values, 3));\n\
         \x20   return 0;\n\
         }\n",
    )
    .unwrap();
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-m", "init"]);
    let repo = Repo::open(dir).unwrap();

    let lsp = std::sync::Arc::new(git_manage::lsp::Manager::new(repo.path(), None));
    let jobs = git_manage::local_ci::discover_configs(repo.path()).unwrap().config.jobs;
    let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
        .unwrap()
        .with_write_mode(WriteMode::Live)
        .with_language_support(lsp)
        .with_checks(jobs);

    let run = coding::run(
        &client,
        &mut ws,
        coding::Request {
            branch: Some("main"),
            ..coding::Request::new(
                "main.c does not build. Find out why, fix it, and make sure the build \
                 check passes.",
            )
        },
        &mut |event| println!("  {}", event.line()),
    )
    .expect("the agent should finish");

    println!("\n--- SUMMARY ---\n{}\n", run.text);
    println!("--- EDITS: {} file(s) ---", run.edits.len());
    for edit in &run.edits {
        let (added, removed) = edit.line_delta();
        println!("  {} +{added} -{removed}", edit.path);
    }

    // The proof is not what it said, but whether the code builds now.
    let result = git_manage::local_ci::run_job(
        repo.path(),
        &git_manage::local_ci::Job {
            name: "verify".into(),
            commands: vec!["cc -Wall -Werror -c main.c -o /dev/null".into()],
            ..Default::default()
        },
    );
    println!("--- VERIFY ---\n{}", result.output);
    assert!(result.ok, "the agent reported done but the code still does not build");
    assert!(!run.edits.is_empty(), "it cannot have fixed anything without editing");
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

/// The verifier against three findings that the code itself refutes, plus one
/// defect the code itself proves.
///
/// Against a fixture, not this repository. The original version seeded
/// findings about the app's own source and asserted the model's verdict on
/// each; two runs a minute apart disagreed about whether a bounded scan of
/// every tracked file is a defect, which is a fair thing to disagree about
/// and a terrible thing to assert. Every finding here is settled by reading
/// one file: three are refuted by the line below the one they quote, and the
/// fourth indexes past the end of a slice.
///
/// Ignored by default — needs Claude credentials and costs tokens.
/// `cargo test --test agent -- --ignored --nocapture live_verifier`
#[test]
#[ignore]
fn live_verifier_drops_what_the_code_refutes() {
    use git_manage::review::{Finding, ReviewConfig, ReviewOutcome, Severity};

    let Some(client) = git_manage::claude::Client::from_store("claude-opus-5") else {
        eprintln!("Claude is not signed in; skipping");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let write = |name: &str, body: &str| {
        std::fs::write(root.join(name), body).unwrap();
    };

    // Refuted by the attribute on the field itself.
    write(
        "config.rs",
        "use serde::Deserialize;\n\
         \n\
         fn default_true() -> bool {\n    true\n}\n\
         \n\
         #[derive(Deserialize)]\n\
         pub struct Config {\n\
         \x20   /// Whether the reviewer may read the repository.\n\
         \x20   #[serde(default = \"default_true\")]\n\
         \x20   pub repo_context: bool,\n\
         }\n",
    );

    // Refuted by the caller two lines down, which supplies the default.
    write(
        "prompt.rs",
        "const DEFAULT_PROMPT: &str = \"You are resolving a merge conflict.\";\n\
         \n\
         /// The project's own instructions, if it has any.\n\
         pub fn custom_prompt() -> Option<String> {\n\
         \x20   std::fs::read_to_string(\".merge-prompt\").ok()\n\
         }\n\
         \n\
         pub fn system_prompt() -> String {\n\
         \x20   custom_prompt().unwrap_or_else(|| DEFAULT_PROMPT.to_string())\n\
         }\n",
    );

    // Refuted by the bounds check on the line above the indexing.
    write(
        "index.rs",
        "pub fn nth(items: &[u32], i: usize) -> Option<u32> {\n\
         \x20   if i >= items.len() {\n\
         \x20       return None;\n\
         \x20   }\n\
         \x20   Some(items[i])\n\
         }\n",
    );

    // A real defect: an inclusive range over indices, which reads one past
    // the end on the last iteration and panics. Nothing in the file argues
    // otherwise.
    write(
        "sum.rs",
        "pub fn total(values: &[u32]) -> u32 {\n\
         \x20   let mut sum = 0;\n\
         \x20   for i in 0..=values.len() {\n\
         \x20       sum += values[i];\n\
         \x20   }\n\
         \x20   sum\n\
         }\n",
    );

    let files: Vec<String> =
        ["config.rs", "prompt.rs", "index.rs", "sum.rs"].iter().map(|s| s.to_string()).collect();
    let mut ws = Workspace::new(root, files, Access::ReadOnly).unwrap();

    let finding = |file: &str, title: &str, detail: &str, evidence: &str| Finding {
        file: file.into(),
        line: None,
        severity: Severity::High,
        title: title.into(),
        detail: detail.into(),
        evidence: evidence.into(),
        verified: false,
    };

    let refuted = [
        "repo_context may be uninitialised when the config omits it",
        "a missing prompt file leaves the model with no instructions",
        "nth indexes a slice with a caller-supplied index",
    ];
    let real = "total indexes one past the end of values";

    let mut outcome = ReviewOutcome {
        summary: "four findings".into(),
        findings: vec![
            finding(
                "config.rs",
                refuted[0],
                "Config::repo_context is a plain bool, so a config file that omits \
                 the key leaves it unset.",
                "    pub repo_context: bool,",
            ),
            finding(
                "prompt.rs",
                refuted[1],
                "custom_prompt returns Option<String>, and a None means the model is \
                 given no instructions at all.",
                "pub fn custom_prompt() -> Option<String> {",
            ),
            finding(
                "index.rs",
                refuted[2],
                "nth indexes items with an index that comes from the caller, which \
                 panics when it is out of range.",
                "    Some(items[i])",
            ),
            finding(
                "sum.rs",
                real,
                "The loop runs to values.len() inclusive, so the last iteration \
                 indexes one element past the end and panics.",
                "    for i in 0..=values.len() {",
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
    for false_positive in refuted {
        assert!(!kept.contains(&false_positive), "kept a false positive: {false_positive}");
    }
    // And it must not simply drop everything: a gate that silences every
    // finding is worse than no gate, because it looks like it is working.
    assert!(kept.contains(&real), "the real defect was dropped: {kept:?}");
    // Every survivor was examined. One kept because the verifier ran out of
    // budget before reaching it has not passed verification, and the two used
    // to be indistinguishable.
    for finding in &outcome.findings {
        assert!(finding.verified, "\"{}\" survived without being checked", finding.title);
    }
}
