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
//! Jira is written to only through a [`Claimer`], and only if the caller
//! gives one: assigning the ticket to the developer, putting it in the
//! active sprint, moving it to In Progress when the agent starts, and
//! commenting with the pull request — or with why it gave up — at the end.
//! None of that can fail the fix; it is logged and carried on from.

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
    /// Which harness did it, as the log announced.
    pub engine: String,
    /// Rounds it took: one means the first attempt passed and was approved.
    pub rounds: usize,
    /// Who reviewed it, when someone did.
    pub reviewed_by: Option<String>,
}

/// Marks a ticket as taken while an agent works on it, and says how it
/// went. Every method returns lines for the log; a failure is one of them.
pub trait Claimer: Sync {
    /// The agent has started: assign, sprint, In Progress.
    fn start(&self, key: &str) -> Vec<String>;
    /// The agent has finished, with a pull request or a reason.
    fn finish(&self, key: &str, outcome: Result<&PullRequest, &str>) -> Vec<String>;
}

/// Claims tickets in Jira on the developer's behalf.
pub struct JiraClaim {
    pub client: crate::jira::Client,
    /// The developer's Jira account, from `myself`.
    pub account_id: String,
    /// The project's active sprint, when it has one.
    pub sprint: Option<crate::jira::Sprint>,
}

impl Claimer for JiraClaim {
    fn start(&self, key: &str) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(match self.client.assign(key, &self.account_id) {
            Ok(()) => format!("{key} assigned to you"),
            Err(e) => format!("could not assign {key}: {e}"),
        });
        if let Some(sprint) = &self.sprint {
            lines.push(match self.client.move_to_sprint(sprint.id, &[key]) {
                Ok(()) => format!("{key} moved to {}", sprint.name),
                Err(e) => format!("could not move {key} to {}: {e}", sprint.name),
            });
        } else {
            lines.push("no active sprint to move it to".into());
        }
        lines.push(match self.client.start_progress(key) {
            Ok(Some(step)) => format!("{key}: {step}"),
            Ok(None) => format!("{key}: no In Progress step in this workflow"),
            Err(e) => format!("could not move {key} to In Progress: {e}"),
        });
        lines
    }

    fn finish(&self, key: &str, outcome: Result<&PullRequest, &str>) -> Vec<String> {
        let comment = match outcome {
            Ok(pr) => format!(
                "A draft pull request for this ticket is ready for review: [#{} {}]({})\n\n\
                 Opened by DevDock's coding agent.",
                pr.number, pr.title, pr.html_url
            ),
            Err(reason) => format!(
                "DevDock's coding agent tried this ticket and could not finish it:\n\n{}\n\n\
                 It is still assigned to you.",
                reason.lines().take(8).collect::<Vec<_>>().join("\n")
            ),
        };
        vec![match self.client.add_comment(key, &comment) {
            Ok(()) => format!("commented on {key}"),
            Err(e) => format!("could not comment on {key}: {e}"),
        }]
    }
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
    /// Marks the ticket as taken in Jira, when the developer wants that.
    pub claim: Option<&'a dyn Claimer>,
    /// How many times the agent may try: a failed check or a reviewer's
    /// "revise" sends it back with the reason, up to this many rounds.
    pub rounds: usize,
    /// A second engine that reads the ticket and the diff before the pull
    /// request and says approve or revise. `None` skips the review.
    pub reviewer: Option<&'a Engine>,
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
pub fn pull_request_text(
    fixed_summary: &str,
    issue: &BacklogIssue,
    checks: &[CheckOutcome],
    engine: &str,
    rounds: usize,
    reviewed_by: Option<&str>,
) -> (String, String) {
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
    match reviewed_by {
        Some(who) => body.push_str(&format!(
            "\nReviewed and approved by {who} after {rounds} round(s).\n"
        )),
        None => body.push_str(&format!("\nNot reviewed by a second agent; {rounds} round(s).\n")),
    }
    body.push_str(&format!(
        "\n---\n*Drafted from the Jira backlog by DevDock, engine: {engine}. Review before \
         marking ready.*\n"
    ));
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
    if let Some(claim) = job.claim {
        for line in claim.start(&job.issue.key) {
            on_event(line);
        }
    }

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
    // The ticket hears how it went last, once everything else is settled.
    if let Some(claim) = job.claim {
        let lines = match &result {
            Ok(fixed) => claim.finish(&job.issue.key, Ok(&fixed.pr)),
            Err(e) => claim.finish(&job.issue.key, Err(e)),
        };
        for line in lines {
            on_event(line);
        }
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
    if jobs.is_empty() {
        // "No config" must not mean "nothing was tested".
        jobs = crate::local_ci::inferred_jobs(wt.path());
        if jobs.is_empty() {
            on_event("no checks declared and none could be inferred; the change will be unverified".into());
        } else {
            let names: Vec<String> = jobs.iter().flat_map(|j| j.commands.clone()).collect();
            on_event(format!("no checks declared; inferred: {}", names.join(", ")));
        }
    }
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

    let tracked = wt.tracked_files().map_err(|e| e.to_string())?;
    let mut workspace = Workspace::new(wt.path(), tracked.clone(), Access::ReadWrite)?
        .with_write_mode(WriteMode::Live)
        .with_checks(jobs.clone());
    let base_task = task_text(job.issue, job.triage);
    let rounds = job.rounds.max(1);
    let mut feedback: Option<String> = None;
    let mut turns = 0;
    let mut summary;
    let mut checks: Vec<CheckOutcome> = Vec::new();
    let mut reviewed_by: Option<String> = None;
    let mut round = 0;
    loop {
        round += 1;
        on_event(format!("round {round} of {rounds}"));
        let task = match &feedback {
            None => base_task.clone(),
            Some(why) => format!(
                "{base_task}\n\nThis is round {round} of {rounds}. Your previous attempt is \
                 in the tree and was not accepted:\n\n{why}\n\nFix that. Do not start over \
                 unless the approach was wrong."
            ),
        };
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
        turns += run.turns;
        summary = run.text;
        if changed_files(&wt)?.is_empty() {
            return Err(format!("the agent changed nothing: {}", first_line(&summary)));
        }

        // The checks, run here. The agent's word that they passed is not
        // what a draft pull request should rest on.
        checks.clear();
        let mut failed: Option<String> = None;
        for j in &jobs {
            on_event(format!("verifying: {}", j.name));
            let result = crate::local_ci::run_job(wt.path(), j);
            on_event(format!("{} {}", j.name, if result.ok { "passed" } else { "FAILED" }));
            checks.push(CheckOutcome { name: j.name.clone(), ok: result.ok });
            if !result.ok {
                let tail: String = result
                    .output
                    .lines()
                    .rev()
                    .take(40)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                failed = Some(format!("The check `{}` fails:\n{tail}", j.name));
                break;
            }
        }
        if let Some(why) = failed {
            if round >= rounds {
                return Err(format!("after {rounds} round(s) the change still fails a check.\n{why}"));
            }
            on_event("sending the failure back to the agent".into());
            feedback = Some(why);
            continue;
        }

        // A second opinion, before anyone else sees it.
        if let Some(reviewer) = job.reviewer {
            on_event(format!("review by {}", reviewer.label()));
            let diff = wt.git(&["diff", job.base]).map_err(|e| e.to_string())?;
            let verdict = review_with(reviewer, wt.path(), &tracked, job.issue, &diff, on_event)?;
            if verdict.approve {
                on_event(format!("approved: {}", first_line(&verdict.feedback)));
                reviewed_by = Some(reviewer.label());
            } else {
                on_event(format!("revise: {}", first_line(&verdict.feedback)));
                if round >= rounds {
                    return Err(format!(
                        "after {rounds} round(s) the reviewer still asked for changes:\n{}",
                        verdict.feedback
                    ));
                }
                feedback = Some(format!("A reviewer read your change and asked for changes:\n{}", verdict.feedback));
                continue;
            }
        }
        break;
    }

    let changes = changed_files(&wt)?;
    for c in &changes {
        on_event(format!("changed {} +{} -{}", c.path, c.added, c.removed));
    }

    // Commit and push.
    wt.stage_all().map_err(|e| e.to_string())?;
    let subject = format!("{}: {}", job.issue.key, truncate(job.issue.summary.trim(), 60));
    let body = format!("{}\n\n{}", summary.trim(), job.issue.url);
    wt.commit(&subject, &body, false).map_err(|e| e.to_string())?;
    on_event(format!("committed: {subject}"));
    wt.push_branch(branch, false, job.auth).map_err(|e| format!("push failed: {e}"))?;
    on_event(format!("pushed {branch}"));

    // The pull request.
    let (title, pr_body) =
        pull_request_text(&summary, job.issue, &checks, &engine.label(), round, reviewed_by.as_deref());
    let pr = publish(&title, &pr_body, branch)?;
    on_event(format!("draft pull request #{} opened", pr.number));

    Ok(Fixed {
        key: job.issue.key.clone(),
        branch: branch.to_string(),
        pr,
        summary,
        changes,
        checks,
        turns,
        engine: engine.label(),
        rounds: round,
        reviewed_by,
    })
}

/// Reviews the change with whichever engine: the harness over a read-only
/// workspace on the changed tree, or Claude Code with reading tools only.
fn review_with(
    reviewer: &Engine,
    root: &Path,
    tracked: &[String],
    issue: &BacklogIssue,
    diff: &str,
    on_event: &mut dyn FnMut(String),
) -> Result<crate::agent::backlog::Verdict, String> {
    match reviewer {
        Engine::Harness(provider) => {
            let mut workspace = Workspace::new(root, tracked.to_vec(), Access::ReadOnly)?;
            crate::agent::backlog::review(provider.as_ref(), &mut workspace, issue, diff, &mut |e| on_event(e.line()))
        }
        Engine::ClaudeCode(config) => {
            let task = crate::agent::backlog::review_task(issue, diff);
            let run = crate::agent::claude_code::run_readonly(
                config,
                root,
                &task,
                Some(
                    "You are reviewing a change an unattended coding agent made, before it \
                     becomes a pull request. Be strict: revise unless you would merge it. \
                     Answer with JSON only: {\"verdict\": \"approve\" | \"revise\", \
                     \"feedback\": \"…\"}",
                ),
                &mut |e| on_event(e.line()),
            )?;
            Ok(crate::agent::backlog::parse_verdict(&run.text))
        }
    }
}

/// What the worktree has changed against its commit, per git, so the
/// report is of the tree and not of what one engine remembers doing.
fn changed_files(wt: &Repo) -> Result<Vec<ChangedFile>, String> {
    wt.stage_all().map_err(|e| e.to_string())?;
    let numstat = wt.git(&["diff", "--cached", "--numstat"]).map_err(|e| e.to_string())?;
    let head_files = wt.git(&["ls-tree", "-r", "--name-only", "HEAD"]).unwrap_or_default();
    let mut changes = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let removed = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let Some(path) = parts.next() else { continue };
        changes.push(ChangedFile {
            path: path.to_string(),
            added,
            removed,
            new: !head_files.lines().any(|f| f == path),
        });
    }
    Ok(changes)
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
    if has("pubspec.yaml") {
        Some("ghcr.io/cirruslabs/flutter:stable")
    } else if has("Cargo.toml") {
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
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 1, reviewer: None },
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
        assert_eq!(fixed.engine, "scripted");
        assert_eq!(log.first().map(String::as_str), Some("branch fix/abc-7-total-is-off-by-one from main"));
        assert!(log.iter().any(|l| l == "engine: scripted"), "{log:?}");

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
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 1, reviewer: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_err();
        assert!(err.contains("still fails a check"), "{err}");
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
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 1, reviewer: None },
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
    fn a_failing_check_is_sent_back_and_the_second_round_can_pass() {
        // The check wants `return 0`; the first attempt gives `sum(xs)`, the
        // second, told why, gives `return 0`.
        let (_tmp, repo) = setup("grep -q 'return 0$' lib.py");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "first try".into(), ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "2".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "return sum(xs)", "new_text": "return 0"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "second try".into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 3, reviewer: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.rounds, 2);
        assert_eq!(fixed.summary, "second try");
        assert!(log.iter().any(|l| l == "sending the failure back to the agent"), "{log:?}");
        assert!(log.iter().any(|l| l == "round 2 of 3"));
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
    }

    #[test]
    fn a_reviewer_can_send_it_back_and_then_approve() {
        let (_tmp, repo) = setup("true");
        let fixer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "fixed".into(), ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "2".into(), name: "write_file".into(), input: serde_json::json!({"path": "test_lib.py", "content": "from lib import total\n\ndef test_total():\n    assert total([1, 2]) == 3\n"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "fixed, with a test".into(), ..Default::default() },
        ]))));
        let reviewer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: r#"{"verdict": "revise", "feedback": "No test covers the off-by-one; add one."}"#.into(), ..Default::default() },
            Reply { text: r#"{"verdict": "approve", "feedback": "Fix and test both present."}"#.into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &fixer,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 3, reviewer: Some(&reviewer) },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.rounds, 2);
        assert_eq!(fixed.reviewed_by.as_deref(), Some("scripted"));
        assert!(log.iter().any(|l| l.starts_with("revise: No test covers")), "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("approved: Fix and test")), "{log:?}");
        let paths: Vec<&str> = fixed.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["lib.py", "test_lib.py"], "both rounds' changes are in the report");
        assert!(fixed.changes[1].new);

        // A reviewer that never approves within the rounds is a failure, and
        // the branch it would have pushed is not there.
        let (_tmp, repo) = setup("true");
        let fixer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "fixed".into(), ..Default::default() },
        ]))));
        let reviewer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: r#"{"verdict": "revise", "feedback": "wrong"}"#.into(), ..Default::default() },
        ]))));
        let err = fix(&repo, &fixer, &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 1, reviewer: Some(&reviewer) }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("reviewer still asked for changes"), "{err}");
        assert!(!repo.branches().unwrap().local.iter().any(|b| b.name.starts_with("fix/")));
    }

    /// What a claimer was told, in order.
    struct Recording(std::sync::Mutex<Vec<String>>);
    impl Claimer for Recording {
        fn start(&self, key: &str) -> Vec<String> {
            self.0.lock().unwrap().push(format!("start {key}"));
            vec![format!("{key} assigned to you")]
        }
        fn finish(&self, key: &str, outcome: Result<&PullRequest, &str>) -> Vec<String> {
            let what = match outcome {
                Ok(pr) => format!("pr #{}", pr.number),
                Err(e) => format!("err {}", e.lines().next().unwrap_or("")),
            };
            self.0.lock().unwrap().push(format!("finish {key} {what}"));
            vec![format!("commented on {key}")]
        }
    }

    #[test]
    fn a_claimed_ticket_is_taken_when_the_agent_starts_and_told_how_it_went() {
        let (_tmp, repo) = setup("true");
        let claim = Recording(std::sync::Mutex::new(Vec::new()));
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        fix(
            &repo,
            &engine,
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: Some(&claim), rounds: 1, reviewer: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap();
        assert_eq!(claim.0.lock().unwrap().as_slice(), ["start ABC-7", "finish ABC-7 pr #42"]);
        let start = log.iter().position(|l| l == "ABC-7 assigned to you").unwrap();
        let worktree = log.iter().position(|l| l.starts_with("worktree /")).unwrap();
        assert!(start > worktree, "claimed once the worktree exists, not before: {log:?}");
        assert_eq!(log.last().map(String::as_str), Some("commented on ABC-7"));

        // A failure is reported to the claimer too.
        let (_tmp, repo) = setup("true");
        let claim = Recording(std::sync::Mutex::new(Vec::new()));
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![Reply { text: "Needs a person.".into(), ..Default::default() }]))));
        let _ = fix(&repo, &engine, &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: Some(&claim), rounds: 1, reviewer: None }, &fake_pr, &mut |_| {});
        assert!(claim.0.lock().unwrap()[1].starts_with("finish ABC-7 err the agent changed nothing"));
    }

    #[test]
    fn a_leftover_branch_is_refused_rather_than_reused() {
        let (_tmp, repo) = setup("true");
        repo.create_branch("fix/abc-7-total-is-off-by-one", false).unwrap();
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![]))));
        let err = fix(&repo, &engine, &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: None, claim: None, rounds: 1, reviewer: None }, &fake_pr, &mut |_| {}).unwrap_err();
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
            &Job { issue: &issue(), triage: None, base: "main", auth: None, instructions: None, sandbox_image: Some("alpine:3"), claim: None, rounds: 1, reviewer: None },
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
        assert_eq!(suggest_sandbox_image(&files(&["pubspec.yaml", "lib/main.dart"])), Some("ghcr.io/cirruslabs/flutter:stable"));
        // Rust wins in a mixed repository: it is the one that most needs a
        // pinned toolchain.
        assert_eq!(suggest_sandbox_image(&files(&["package.json", "Cargo.toml"])), Some("rust:1-bookworm"));
    }

    #[test]
    fn the_pull_request_text_quotes_the_ticket_and_the_checks() {
        let (title, body) = pull_request_text("- fixed it", &issue(), &[CheckOutcome { name: "tests".into(), ok: true }], "Claude Code", 2, Some("Claude (opus)"));
        assert_eq!(title, "ABC-7: total() is off by one");
        assert!(body.contains("Resolves [ABC-7](https://acme.atlassian.net/browse/ABC-7)"));
        assert!(body.contains("> It adds 1."));
        assert!(body.contains("- fixed it"));
        assert!(body.contains("✅ `tests`"));
        assert!(body.contains("engine: Claude Code"), "{body}");
        assert!(body.contains("approved by Claude (opus) after 2 round(s)"), "{body}");
        let (_, none) = pull_request_text("x", &issue(), &[], "scripted", 1, None);
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
