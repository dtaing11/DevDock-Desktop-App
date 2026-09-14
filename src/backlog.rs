//! Fixing a Jira ticket unattended: a worktree, the coding agent, the
//! repository's own checks, a commit, a push, and a draft pull request.
//!
//! One ticket is one [`fix`] call, and several can run at the same time,
//! each in its own worktree so none of them can see another's half-written
//! files. The worktree is temporary: it is removed once the branch is pushed,
//! and the branch is what the draft pull request is made from. A run that
//! produced nothing, or whose changes failed the repository's checks, leaves
//! nothing behind but its log — no branch, no worktree.
//!
//! Nothing here touches Jira. The ticket is read; the developer decides what
//! to do with the pull request.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::agent::backlog::Triage;
use crate::agent::coding::{self, Engine};
use crate::agent::{Access, Event, Workspace, WriteMode};
use crate::git::Repo;
use crate::github::PullRequest;
use crate::jira::BacklogIssue;

/// `git worktree add` on the same repository from several threads at once
/// races on the administrative directory. One at a time costs nothing.
static WORKTREE_LOCK: Mutex<()> = Mutex::new(());

/// One file the run changed, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub new: bool,
}

/// One check the run's result was put through, run by the harness after
/// the agent said it was done — never by the agent's own account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub name: String,
    pub ok: bool,
}

/// A ticket fixed and published.
#[derive(Debug, Clone)]
pub struct Fixed {
    pub key: String,
    pub branch: String,
    pub pr: PullRequest,
    /// The agent's closing summary.
    pub summary: String,
    pub changes: Vec<ChangedFile>,
    pub checks: Vec<CheckOutcome>,
    pub turns: usize,
}

/// What the fix needs from the outside.
pub struct Job<'a> {
    pub issue: &'a BacklogIssue,
    pub triage: Option<&'a Triage>,
    /// The branch the fix starts from and the pull request targets.
    pub base: &'a str,
    /// GitHub token for the push; `None` uses whatever git has.
    pub auth: Option<&'a str>,
    /// Project guidance for the agent (review instructions, say).
    pub instructions: Option<&'a str>,
    /// A Docker image to run every check in, with the worktree mounted at
    /// /work, so a build or a test suite the agent triggers cannot touch
    /// the machine. Checks that already name an image keep theirs.
    pub sandbox_image: Option<&'a str>,
}

/// The branch a ticket's fix lives on: `fix/abc-7-crash-on-empty-repo`.
pub fn branch_name(issue: &BacklogIssue) -> String {
    let key = issue.key.trim().to_lowercase();
    let mut slug = String::new();
    for c in issue.summary.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("fix/{key}")
    } else {
        format!("fix/{key}-{slug}")
    }
}

/// The instruction the agent gets: the ticket, what triage found, and the
/// rules of an unattended run.
pub fn task_text(issue: &BacklogIssue, triage: Option<&Triage>) -> String {
    let mut text = format!(
        "Resolve this Jira ticket. Nobody can answer questions during the run: decide \
         for yourself, state any assumption in your summary, and keep the change to \
         what the ticket asks.\n\n{}\n",
        issue.prompt_text(6_000)
    );
    if let Some(t) = triage {
        if !t.area.is_empty() {
            text.push_str(&format!("\nThe work is in {}/.\n", t.area));
        }
        if !t.plan.is_empty() {
            text.push_str(&format!("\nA first look suggested:\n{}\n", t.plan));
        }
    }
    text.push_str(
        "\nRun the repository's checks before you finish. If the ticket cannot be done \
         without a decision from a person, say so in your summary and change nothing.",
    );
    text
}

/// The pull request's title and body.
pub fn pull_request_text(fixed_summary: &str, issue: &BacklogIssue, checks: &[CheckOutcome]) -> (String, String) {
    let title = format!("{}: {}", issue.key, issue.summary.trim());
    let mut body = format!("Resolves [{}]({}).\n\n", issue.key, issue.url);
    if !issue.description.trim().is_empty() {
        let quoted: Vec<String> = issue
            .description
            .trim()
            .lines()
            .take(30)
            .map(|l| format!("> {l}"))
            .collect();
        body.push_str(&quoted.join("\n"));
        body.push_str("\n\n");
    }
    body.push_str("## What changed\n\n");
    body.push_str(fixed_summary.trim());
    body.push_str("\n\n## Verified\n\n");
    if checks.is_empty() {
        body.push_str("This repository declares no checks (`.git-manage-ci.toml`), so nothing was run.\n");
    } else {
        for c in checks {
            body.push_str(&format!("- {} `{}`\n", if c.ok { "✅" } else { "❌" }, c.name));
        }
    }
    body.push_str(
        "\n---\n*Drafted by DevDock's coding agent from the Jira backlog. Review before \
         marking ready.*\n",
    );
    (title, body)
}

/// Fixes one ticket end to end. `publish` opens the pull request from the
/// pushed branch, so a test can stand in a fake GitHub.
///
/// Steps, and what a failure at each leaves behind:
/// 1. a branch and a worktree from `base` — nothing yet;
/// 2. the coding agent, live, with the repository's checks — the worktree
///    is removed and the branch deleted if it changed nothing;
/// 3. the checks, run again here — same, if any fails;
/// 4. a commit and a push — the branch stays if the push fails, so the work
///    is not lost; the worktree goes either way;
/// 5. the pull request — the branch stays.
pub fn fix(
    repo: &Repo,
    engine: &Engine,
    job: &Job<'_>,
    publish: &dyn Fn(&str, &str, &str) -> Result<PullRequest, String>,
    on_event: &mut dyn FnMut(String),
) -> Result<Fixed, String> {
    let branch = branch_name(job.issue);
    let dir = repo.worktree_default_path(&branch);
    on_event(format!("branch {branch} from {}", job.base));

    // 1. Worktree.
    {
        let _guard = WORKTREE_LOCK.lock().map_err(|_| "worktree lock poisoned".to_string())?;
        let exists = repo.branches().map(|b| b.local.iter().any(|br| br.name == branch)).unwrap_or(false);
        if exists {
            return Err(format!(
                "branch {branch} already exists; a previous attempt left it. Delete it or \
                 open its pull request."
            ));
        }
        repo.worktree_add(&dir, &branch, Some(job.base)).map_err(|e| e.to_string())?;
    }
    on_event(format!("worktree {}", dir.display()));

    let result = work(engine, job, &branch, &dir, publish, on_event);

    // The worktree is temporary whatever happened.
    {
        let _guard = WORKTREE_LOCK.lock().map_err(|_| "worktree lock poisoned".to_string())?;
        if let Err(e) = repo.worktree_remove(&dir, true) {
            on_event(format!("could not remove the worktree: {e}"));
        } else {
            on_event("worktree removed".into());
        }
    }
    if result.is_err() && !branch_has_commits(repo, &branch, job.base) {
        let _ = repo.delete_branch(&branch, true);
        on_event(format!("branch {branch} deleted: nothing was kept"));
    }
    result
}

fn work(
    engine: &Engine,
    job: &Job<'_>,
    branch: &str,
    dir: &Path,
    publish: &dyn Fn(&str, &str, &str) -> Result<PullRequest, String>,
    on_event: &mut dyn FnMut(String),
) -> Result<Fixed, String> {
    let wt = Repo::open(dir).map_err(|e| e.to_string())?;
    let mut jobs = crate::local_ci::discover_configs(wt.path())
        .map(|c| c.config.jobs)
        .unwrap_or_default();
    if let Some(image) = job.sandbox_image.map(str::trim).filter(|i| !i.is_empty()) {
        if !crate::local_ci::docker_available() {
            return Err(format!(
                "checks are set to run in the {image} sandbox, but Docker is not available \
                 on this machine"
            ));
        }
        for j in jobs.iter_mut() {
            if j.image.is_none() {
                j.image = Some(image.to_string());
            }
        }
        on_event(format!("checks run in the {image} sandbox"));
    }

    // 2. The agent.
    let tracked = wt.tracked_files().map_err(|e| e.to_string())?;
    let mut workspace = Workspace::new(wt.path(), tracked, Access::ReadWrite)?
        .with_write_mode(WriteMode::Live)
        .with_checks(jobs.clone());
    let task = task_text(job.issue, job.triage);
    on_event(format!("engine: {}", engine.label()));
    let run = coding::run_with(
        engine,
        &mut workspace,
        coding::Request {
            branch: Some(branch),
            instructions: job.instructions,
            context: Some("This is an unattended run on a fresh worktree of the repository."),
            ..coding::Request::new(&task)
        },
        &mut |event: Event| on_event(event.line()),
    )?;
    if run.edits.is_empty() {
        return Err(format!("the agent changed nothing: {}", first_line(&run.text)));
    }
    let changes: Vec<ChangedFile> = run
        .edits
        .iter()
        .map(|e| {
            let (added, removed) = e.line_delta();
            ChangedFile { path: e.path.clone(), added, removed, new: e.is_new() }
        })
        .collect();
    for c in &changes {
        on_event(format!("changed {} +{} -{}", c.path, c.added, c.removed));
    }

    // 3. The checks, run here. The agent's word that they passed is not
    // what a draft pull request should rest on.
    let mut checks = Vec::new();
    for j in &jobs {
        on_event(format!("verifying: {}", j.name));
        let result = crate::local_ci::run_job(wt.path(), j);
        on_event(format!("{} {}", j.name, if result.ok { "passed" } else { "FAILED" }));
        checks.push(CheckOutcome { name: j.name.clone(), ok: result.ok });
        if !result.ok {
            let tail: String = result.output.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            return Err(format!("the change fails the repository's check `{}`:\n{tail}", j.name));
        }
    }

    // 4. Commit and push.
    wt.stage_all().map_err(|e| e.to_string())?;
    let subject = format!("{}: {}", job.issue.key, truncate(job.issue.summary.trim(), 60));
    let body = format!("{}\n\n{}", run.text.trim(), job.issue.url);
    wt.commit(&subject, &body, false).map_err(|e| e.to_string())?;
    on_event(format!("committed: {subject}"));
    wt.push_branch(branch, false, job.auth).map_err(|e| format!("push failed: {e}"))?;
    on_event(format!("pushed {branch}"));

    // 5. The pull request.
    let (title, pr_body) = pull_request_text(&run.text, job.issue, &checks);
    let pr = publish(&title, &pr_body, branch)?;
    on_event(format!("draft pull request #{} opened", pr.number));

    Ok(Fixed {
        key: job.issue.key.clone(),
        branch: branch.to_string(),
        pr,
        summary: run.text,
        changes,
        checks,
        turns: run.turns,
    })
}

fn branch_has_commits(repo: &Repo, branch: &str, base: &str) -> bool {
    repo.git(&["rev-list", "--count", &format!("{base}..{branch}")])
        .ok()
        .and_then(|o| o.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0)
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().chars().take(120).collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max - 1).collect::<String>() + "…"
    }
}

/// Where a worktree for the ticket would go, for the UI.
pub fn worktree_path(repo: &Repo, issue: &BacklogIssue) -> PathBuf {
    repo.worktree_default_path(&branch_name(issue))
}

/// A Docker image that can build and test this repository, judged from its
/// project files, for the sandbox setting's default. `None` when there is no
/// obvious toolchain, in which case the user names one.
pub fn suggest_sandbox_image(tracked: &[String]) -> Option<&'static str> {
    let has = |name: &str| tracked.iter().any(|t| t == name || t.ends_with(&format!("/{name}")));
    if has("Cargo.toml") {
        Some("rust:1-bookworm")
    } else if has("package.json") {
        Some("node:22-bookworm")
    } else if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        Some("python:3.12-bookworm")
    } else if has("go.mod") {
        Some("golang:1.23-bookworm")
    } else if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        Some("eclipse-temurin:21")
    } else if has("Gemfile") {
        Some("ruby:3.3-bookworm")
    } else if has("mix.exs") {
        Some("elixir:1.17")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, Provider, Reply, ToolCall, ToolSpec};
    use std::cell::RefCell;
    use std::fs;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repository with a bare remote, a check that greps for a fix, and a
    /// file to fix.
    fn setup(check: &str) -> (tempfile::TempDir, Repo) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("app");
        let bare = tmp.path().join("remote.git");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&bare).unwrap();
        sh(&bare, &["init", "--bare"]);
        sh(&work, &["init", "-b", "main"]);
        sh(&work, &["config", "user.email", "t@t.io"]);
        sh(&work, &["config", "user.name", "T"]);
        sh(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fs::write(work.join("lib.py"), "def total(xs):\n    return sum(xs) + 1\n").unwrap();
        fs::write(
            work.join(".git-manage-ci.toml"),
            format!("[[job]]\nname = \"tests\"\ncommands = [\"{check}\"]\n"),
        )
        .unwrap();
        sh(&work, &["add", "-A"]);
        sh(&work, &["commit", "-q", "-m", "init"]);
        sh(&work, &["push", "-q", "origin", "main"]);
        (tmp, Repo::open(&work).unwrap())
    }

    struct Scripted(RefCell<Vec<Reply>>);
    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(&self, _: &str, _: &[Message], _: &[ToolSpec], _: u32) -> Result<Reply, String> {
            Ok(self.0.borrow_mut().remove(0))
        }
    }

    fn issue() -> BacklogIssue {
        BacklogIssue {
            key: "ABC-7".into(),
            summary: "total() is off by one".into(),
            description: "It adds 1.".into(),
            url: "https://acme.atlassian.net/browse/ABC-7".into(),
            ..Default::default()
        }
    }

    fn fixing_provider() -> Scripted {
        Scripted(RefCell::new(vec![
            Reply {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "1".into(),
                    name: "edit_file".into(),
                    input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}),
                }],
                ..Default::default()
            },
            Reply {
                text: String::new(),
                calls: vec![ToolCall { id: "2".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }],
                ..Default::default()
            },
            Reply { text: "- lib.py: dropped the stray + 1\n\nVerified: tests".into(), ..Default::default() },
        ]))
    }

    fn fake_pr(title: &str, body: &str, head: &str) -> Result<PullRequest, String> {
        Ok(PullRequest {
            number: 42,
            title: title.into(),
            html_url: format!("https://github.com/x/y/pull/42?{body_len}", body_len = body.len()),
            state: "open".into(),
            head: head.into(),
            head_sha: String::new(),
            base: "main".into(),
            user: "bot".into(),
        })
    }

    #[test]
    fn branch_names_are_short_and_safe() {
        let mut i = issue();
        assert_eq!(branch_name(&i), "fix/abc-7-total-is-off-by-one");
        i.summary = "  !!! ".into();
        assert_eq!(branch_name(&i), "fix/abc-7");
        i.summary = "x".repeat(200);
        assert!(branch_name(&i).len() < 60);
    }

    #[test]
    fn a_fix_ends_as_a_pushed_branch_and_a_pull_request_with_no_worktree_left() {
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));

        assert_eq!(fixed.branch, "fix/abc-7-total-is-off-by-one");
        assert_eq!(fixed.pr.number, 42);
        assert_eq!(fixed.changes.len(), 1);
        assert_eq!(fixed.changes[0].path, "lib.py");
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        assert!(fixed.pr.title.starts_with("ABC-7: total() is off by one"));

        // The branch is on the remote, the main checkout is untouched, and
        // the worktree is gone.
        let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
        assert!(remote.contains("refs/heads/fix/abc-7-total-is-off-by-one"), "{remote}");
        assert_eq!(fs::read_to_string(repo.path().join("lib.py")).unwrap(), "def total(xs):\n    return sum(xs) + 1\n");
        assert_eq!(repo.worktrees().unwrap().len(), 1, "{:?}", repo.worktrees());
        assert!(!worktree_path(&repo, &issue()).exists());
        assert!(log.iter().any(|l| l.contains("worktree removed")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("changed lib.py +1 -1")), "{log:?}");
        let subject = repo.log(1, Some("fix/abc-7-total-is-off-by-one")).unwrap()[0].subject.clone();
        assert_eq!(subject, "ABC-7: total() is off by one");
    }

    #[test]
    fn a_change_that_fails_the_check_leaves_nothing_behind() {
        // The check wants something the fix does not do.
        let (_tmp, repo) = setup("grep -q 'return 0$' lib.py");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        // The agent's own run_check will fail too; it answers anyway.
        let mut log = Vec::new();
        let err = fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_err();
        assert!(err.contains("fails the repository's check"), "{err}");
        assert!(!repo.branches().unwrap().local.iter().any(|b| b.name.starts_with("fix/")), "the branch was kept");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn an_agent_that_changes_nothing_is_a_failure_not_a_pull_request() {
        let (_tmp, repo) = setup("true");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![Reply { text: "This needs a product decision.".into(), ..Default::default() }]))));
        let err = fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None },
            &fake_pr,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("changed nothing"), "{err}");
        assert!(err.contains("product decision"), "{err}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
        assert!(!repo.branches().unwrap().local.iter().any(|b| b.name.starts_with("fix/")));
    }

    #[test]
    fn a_leftover_branch_is_refused_rather_than_reused() {
        let (_tmp, repo) = setup("true");
        repo.create_branch("fix/abc-7-total-is-off-by-one", false).unwrap();
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![]))));
        let err = fix(&repo, &engine, &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn a_sandbox_without_docker_is_refused_before_anything_runs() {
        if crate::local_ci::docker_available() {
            eprintln!("skipped: docker is available here");
            return;
        }
        let (_tmp, repo) = setup("true");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let err = fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: Some("alpine:3") },
            &fake_pr,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("Docker is not available"), "{err}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn the_sandbox_image_follows_the_toolchain() {
        let files = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(suggest_sandbox_image(&files(&["Cargo.toml", "src/lib.rs"])), Some("rust:1-bookworm"));
        assert_eq!(suggest_sandbox_image(&files(&["app/package.json"])), Some("node:22-bookworm"));
        assert_eq!(suggest_sandbox_image(&files(&["pyproject.toml"])), Some("python:3.12-bookworm"));
        assert_eq!(suggest_sandbox_image(&files(&["README.md"])), None);
        // Rust wins in a mixed repository: it is the one that most needs a
        // pinned toolchain.
        assert_eq!(suggest_sandbox_image(&files(&["package.json", "Cargo.toml"])), Some("rust:1-bookworm"));
    }

    #[test]
    fn the_pull_request_text_quotes_the_ticket_and_the_checks() {
        let (title, body) = pull_request_text("- fixed it", &issue(), &[CheckOutcome { name: "tests".into(), ok: true }]);
        assert_eq!(title, "ABC-7: total() is off by one");
        assert!(body.contains("Resolves [ABC-7](https://acme.atlassian.net/browse/ABC-7)"));
        assert!(body.contains("> It adds 1."));
        assert!(body.contains("- fixed it"));
        assert!(body.contains("✅ `tests`"));
        let (_, none) = pull_request_text("x", &issue(), &[]);
        assert!(none.contains("declares no checks"));
    }

    #[test]
    fn the_task_carries_the_triage_plan() {
        let t = Triage { key: "ABC-7".into(), in_scope: true, area: "src/cli".into(), autonomous: true, confidence: 80, reason: "r".into(), plan: "edit cli.rs".into() };
        let text = task_text(&issue(), Some(&t));
        assert!(text.contains("ABC-7: total() is off by one"));
        assert!(text.contains("The work is in src/cli/."));
        assert!(text.contains("edit cli.rs"));
        assert!(text.contains("Nobody can answer questions"));
    }
}
