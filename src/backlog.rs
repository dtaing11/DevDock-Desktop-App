//! Fixing something unattended: a worktree, the coding agent, the
//! repository's own checks, a commit, a push, and a draft pull request.
//!
//! The something is a [`Task`]: a Jira ticket from the backlog, or a prompt
//! typed into the Agent tab. One task is one [`fix`] call, and several can run at the same time,
//! each in its own worktree so none of them can see another's half-written
//! files. The worktree is temporary: it is removed once the branch is pushed,
//! and the branch is what the draft pull request is made from. A run that
//! produced nothing, or whose changes failed the repository's checks, leaves
//! nothing behind but its log — no branch, no worktree.
//!
//! A round that does not get through is not the end. The agents confer:
//! when the fixer changed nothing or a check failed, a second agent — the
//! reviewer's engine, or the fixer's own in a fresh session — reads the
//! attempt and the repository and says what to do next, or that a person is
//! needed; the fixer's next round gets that, with the thread so far. It
//! stops when the checks pass and the reviewer approves, when the rounds
//! run out, when the advisor says it needs a person, or when a round
//! changes nothing the previous one did not.
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
    /// Checks that already failed on the base branch before any change,
    /// and so were not held against it.
    pub skipped: Vec<String>,
    /// Screenshots of the result, for a change to something with a screen:
    /// a Flutter app's first frame, rendered where the checks ran.
    pub screenshots: Vec<PathBuf>,
    /// For each screenshot that could not be taken, why: shown on the
    /// card where the picture would have been.
    pub no_screenshots: Vec<String>,
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

/// What an agent is asked to do in a worktree of its own: a Jira ticket,
/// or a prompt typed into the Agent tab. The fixer does not care which; the
/// pull request's wording does.
#[derive(Debug, Clone, Default)]
pub struct Task {
    /// The ticket key, or `agent` for a prompt: what the log and the commit
    /// prefix say.
    pub label: String,
    /// One line: the ticket's summary, or the prompt's first line.
    pub title: String,
    /// The ticket's description, or the whole prompt, for the pull request.
    pub description: String,
    /// What the agent and the reviewer read: the ticket with its metadata,
    /// or the prompt as typed.
    pub brief: String,
    /// The ticket's page; a prompt has none.
    pub url: Option<String>,
    /// The branch the work goes on.
    pub branch: String,
    /// What triage found, when the task came through it.
    pub triage: Option<Triage>,
    /// Images the developer attached to a prompt.
    pub images: Vec<crate::agent::Attachment>,
    /// What was asked and what happened before this, on the same branch:
    /// set when the task is a reply to a run that did not get through, and
    /// the run then continues on that branch with what the attempt left
    /// instead of refusing because the branch exists.
    pub earlier: Vec<coding::Turn>,
}

impl Task {
    /// A ticket from the backlog.
    pub fn from_issue(issue: &BacklogIssue, triage: Option<&Triage>) -> Self {
        Self {
            label: issue.key.clone(),
            title: issue.summary.trim().to_string(),
            description: issue.description.trim().to_string(),
            brief: issue.prompt_text(6_000),
            url: Some(issue.url.clone()),
            branch: branch_name(issue),
            triage: triage.cloned(),
            images: Vec::new(),
            earlier: Vec::new(),
        }
    }

    /// A prompt from the Agent tab. The branch is `agent/<slug of the first
    /// line>`; the caller may rename it.
    pub fn from_prompt(prompt: &str) -> Self {
        let prompt = prompt.trim();
        let first = prompt.lines().next().unwrap_or_default().trim();
        let title = truncate(first, 72);
        let slug = slugify(first, 40);
        Self {
            label: "agent".into(),
            title: title.clone(),
            description: prompt.to_string(),
            brief: prompt.to_string(),
            url: None,
            branch: if slug.is_empty() { "agent/task".into() } else { format!("agent/{slug}") },
            triage: None,
            images: Vec::new(),
            earlier: Vec::new(),
        }
    }

    /// This task as a reply to a run on `branch` that did not get through:
    /// the same title, so the commit and the pull request are still named
    /// for the work and not for "continue"; the reply added to the brief;
    /// and what was asked and what happened as the conversation before it.
    pub fn reply_to(original: &str, branch: &str, what_happened: &str, reply: &str) -> Self {
        let mut task = Self::from_prompt(original);
        task.branch = branch.to_string();
        task.brief = format!("{}\n\nThe developer's follow-up, which is what to do now:\n{}", original.trim(), reply.trim());
        task.earlier = vec![coding::Turn {
            task: original.trim().to_string(),
            summary: format!(
                "(this did not get through: {}. What you had changed is on this branch, uncommitted — read it before redoing anything.)",
                what_happened.lines().next().unwrap_or("").trim()
            ),
        }];
        task
    }

    /// A branch name as a person typed it, made valid: spaces and other
    /// characters git refuses become dashes, runs of them collapse, and an
    /// empty result falls back to the name made from the prompt.
    pub fn with_branch(mut self, typed: &str) -> Self {
        let mut name = String::new();
        for c in typed.trim().chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-') {
                name.push(c);
            } else if !name.ends_with('-') && !name.is_empty() {
                name.push('-');
            }
        }
        let name = name.trim_matches(|c| c == '-' || c == '/' || c == '.').replace("//", "/").replace("..", ".");
        if !name.is_empty() {
            self.branch = name;
        }
        self
    }

    /// Whether this is a ticket, with a page to link and a key to prefix.
    pub fn is_ticket(&self) -> bool {
        self.url.is_some()
    }

    /// The pull request's title and the commit's subject: `KEY: summary`
    /// for a ticket, the first line for a prompt.
    pub fn subject(&self) -> String {
        if self.is_ticket() {
            format!("{}: {}", self.label, self.title)
        } else {
            self.title.clone()
        }
    }
}

/// Lowercase letters and digits, runs of anything else collapsed to one
/// dash, cut at `max` bytes.
fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= max {
            break;
        }
    }
    slug.trim_matches('-').to_string()
}

/// What the fix needs from the outside.
pub struct Job<'a> {
    pub task: &'a Task,
    /// The branch the fix starts from and the pull request targets.
    pub base: &'a str,
    /// GitHub token for the push; `None` uses whatever git has.
    pub auth: Option<&'a str>,
    /// Project guidance for the agent (review instructions, say).
    pub instructions: Option<&'a str>,
    /// A machine of the run's own for every check and command — a Lima VM,
    /// an Apple container, or Docker — with the network on and what the
    /// agent installs kept for next time. `None` runs on the host.
    pub sandbox: Option<&'a crate::sandbox::Spec>,
    /// Marks the ticket as taken in Jira, when the developer wants that.
    pub claim: Option<&'a dyn Claimer>,
    /// How many times the agent may try: a failed check or a reviewer's
    /// "revise" sends it back with the reason, up to this many rounds.
    pub rounds: usize,
    /// A second engine that reads the ticket and the diff before the pull
    /// request and says approve or revise. `None` skips the review.
    pub reviewer: Option<&'a Engine>,
    /// A line to the developer for a question the agent cannot decide,
    /// when someone is there to answer. `None` is a fully unattended run.
    pub ask: Option<crate::agent::Asker>,
}

/// The branch a ticket's fix lives on: `fix/abc-7-crash-on-empty-repo`.
pub fn branch_name(issue: &BacklogIssue) -> String {
    let key = issue.key.trim().to_lowercase();
    let slug = slugify(&issue.summary, 40);
    if slug.is_empty() {
        format!("fix/{key}")
    } else {
        format!("fix/{key}-{slug}")
    }
}

/// The instruction the agent gets: the task, what triage found, and the
/// rules of an unattended run.
pub fn task_text(task: &Task, can_ask: bool) -> String {
    let what = if task.is_ticket() { "Resolve this Jira ticket" } else { "Do this task" };
    let asks = if task.is_ticket() { "the ticket" } else { "the task" };
    let rules = if can_ask {
        "If something genuinely uncertain would change what you build, ask the developer \
         with ask_developer — one specific question, with the options you see. Everything \
         else, decide for yourself and state the assumption in your summary."
    } else {
        "Nobody can answer questions during the run: decide for yourself, state any \
         assumption in your summary"
    };
    let mut text = format!("{what}. {rules}, and keep the change to what {asks} asks.\n\n{}\n", task.brief);
    if let Some(t) = &task.triage {
        if !t.area.is_empty() {
            text.push_str(&format!("\nThe work is in {}/.\n", t.area));
        }
        if !t.plan.is_empty() {
            text.push_str(&format!("\nA first look suggested:\n{}\n", t.plan));
        }
    }
    text.push_str(
        "\nRun the repository's checks before you finish. If this cannot be done \
         without a decision from a person, say so in your summary and change nothing.",
    );
    text
}

/// The pull request's title and body.
pub fn pull_request_text(
    fixed_summary: &str,
    task: &Task,
    checks: &[CheckOutcome],
    skipped: &[String],
    engine: &str,
    rounds: usize,
    reviewed_by: Option<&str>,
) -> (String, String) {
    let title = task.subject();
    let quoted = |text: &str| -> String {
        text.trim().lines().take(30).map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
    };
    let mut body = match &task.url {
        Some(url) => {
            let mut b = format!("Resolves [{}]({url}).\n\n", task.label);
            if !task.description.trim().is_empty() {
                b.push_str(&quoted(&task.description));
                b.push_str("\n\n");
            }
            b
        }
        None => format!("Asked in DevDock's Agent tab:\n\n{}\n\n", quoted(&task.description)),
    };
    body.push_str("## What changed\n\n");
    body.push_str(fixed_summary.trim());
    body.push_str("\n\n## Verified\n\n");
    if checks.is_empty() && skipped.is_empty() {
        body.push_str("This repository declares no checks (`.git-manage-ci.toml`), so nothing was run.\n");
    } else {
        for c in checks {
            body.push_str(&format!("- {} `{}`\n", if c.ok { "✅" } else { "❌" }, c.name));
        }
        for name in skipped {
            match name.strip_suffix(UNFINISHED) {
                Some(name) => body.push_str(&format!("- ⏭ `{name}` — does not finish on the base branch where the checks ran; not run\n")),
                None => body.push_str(&format!("- ⏭ `{name}` — fails on the base branch and fails the same way after this change; not counted\n")),
            }
        }
    }
    match reviewed_by {
        Some(who) => body.push_str(&format!(
            "\nReviewed and approved by {who} after {rounds} round(s).\n"
        )),
        None => body.push_str(&format!("\nNot reviewed by a second agent; {rounds} round(s).\n")),
    }
    let from = if task.is_ticket() { "from the Jira backlog" } else { "in a worktree of its own" };
    body.push_str(&format!(
        "\n---\n*Drafted {from} by DevDock's coding agent, engine: {engine}. Review before \
         marking ready.*\n"
    ));
    (title, body)
}

/// Fixes one task end to end. `publish` opens the pull request from the
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
    let branch = job.task.branch.clone();
    if branch.trim().is_empty() || repo.git(&["check-ref-format", "--branch", &branch]).is_err() {
        return Err(format!("{branch:?} is not a valid branch name"));
    }
    let dir = repo.worktree_default_path(&branch);
    on_event(format!("devdock {}", crate::build_description()));
    on_event(format!("branch {branch} from {}", job.base));

    // 1. Worktree.
    {
        let _guard = WORKTREE_LOCK.lock().map_err(|_| "worktree lock poisoned".to_string())?;
        let exists = repo.branches().map(|b| b.local.iter().any(|br| br.name == branch)).unwrap_or(false);
        if exists && job.task.earlier.is_empty() {
            return Err(format!(
                "branch {branch} already exists; a previous attempt left it. Reply on its card to \
                 continue it, delete it (git branch -D {branch}), finish it by hand, or open its pull request."
            ));
        }
        if exists {
            // A reply to a run that did not get through: the same branch,
            // with what the attempt left. Its work-in-progress commit is
            // taken back off, so the changes are the run's to finish and
            // the branch ends with one real commit, not two.
            repo.worktree_add(&dir, &branch, None).map_err(|e| format!("could not continue on {branch}: {e}"))?;
            on_event(format!("continuing on {branch}, with what the last attempt left"));
            if let Ok(wt) = Repo::open(&dir) {
                let subject = wt.git(&["log", "-1", "--format=%s"]).unwrap_or_default();
                if subject.starts_with(WIP_PREFIX) {
                    let _ = wt.git(&["reset", "-q", "--mixed", "HEAD~1"]);
                }
            }
        } else {
            repo.worktree_add(&dir, &branch, Some(job.base)).map_err(|e| e.to_string())?;
        }
    }
    on_event(format!("worktree {}", dir.display()));
    if let Some(claim) = job.claim {
        for line in claim.start(&job.task.label) {
            on_event(line);
        }
    }

    let mut result = work(repo, engine, job, &branch, &dir, publish, on_event);
    // Stopped is not failed: say so first, whatever step the stop landed in.
    if crate::cancel::token(&dir).is_stopped() {
        result = result.map_err(|e| if crate::cancel::was_stopped(&e) { e } else { format!("{}.\n{e}", crate::cancel::STOPPED) });
    }
    crate::cancel::reset(&dir);

    // A failed run's attempt is not thrown away: it is committed on the
    // branch, unpushed, so there is something to finish by hand or to send
    // back. Only the worktree goes.
    if let Err(why) = &result {
        if let Some(line) = keep_attempt(&dir, job.task, why) {
            on_event(line.clone());
            result = Err(format!("{why}\n\n{line}"));
        }
    }

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
            Ok(fixed) => claim.finish(&job.task.label, Ok(&fixed.pr)),
            Err(e) => claim.finish(&job.task.label, Err(e)),
        };
        for line in lines {
            on_event(line);
        }
    }
    result
}

fn work(
    repo: &Repo,
    engine: &Engine,
    job: &Job<'_>,
    branch: &str,
    dir: &Path,
    publish: &dyn Fn(&str, &str, &str) -> Result<PullRequest, String>,
    on_event: &mut dyn FnMut(String),
) -> Result<Fixed, String> {
    let wt = Repo::open(dir).map_err(|e| e.to_string())?;
    let budget = Budget::new(wt.path());
    if job.sandbox.is_none() {
        if let Some(line) = share_cargo_target(wt.path(), repo.path()) {
            on_event(line);
        }
    }
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
    // The sandbox, when there is one: started once, shared by the agent's
    // commands and the verification here, gone when this returns.
    let sandbox = match job.sandbox {
        Some(spec) => {
            let sandbox = crate::sandbox::Sandbox::start(spec, wt.path(), on_event)?;
            // What the checks and the repository's toolchains run, installed
            // where they will run. A plain image has none of it.
            let mut programs: Vec<&str> = crate::local_ci::toolchain_commands(wt.path());
            let firsts: Vec<String> = jobs.iter().flat_map(|j| j.commands.iter()).filter_map(|c| c.split_whitespace().next()).filter(|w| *w != "cd").map(str::to_string).collect();
            programs.extend(firsts.iter().map(String::as_str));
            programs.sort_unstable();
            programs.dedup();
            sandbox.provision(&programs, on_event)?;
            Some(std::sync::Arc::new(sandbox))
        }
        None => None,
    };
    let mut runners = crate::local_ci::runner::RunnerRegistry::with_builtins();
    if let Some(sandbox) = &sandbox {
        runners.register(Box::new(crate::sandbox::SandboxRunner(sandbox.clone())));
        for j in jobs.iter_mut() {
            j.runner = Some(crate::sandbox::RUNNER_ID.into());
            j.image = None;
        }
    }
    let runners = std::sync::Arc::new(runners);

    // The worktree made ready: dependencies fetched, where the checks will
    // run. A failure here is the environment's, reported as such, before
    // any round is spent.
    for mut step in crate::local_ci::prepare_jobs(wt.path(), sandbox.is_some()) {
        if sandbox.is_some() {
            step.runner = Some(crate::sandbox::RUNNER_ID.into());
        }
        step.timeout_secs = Some(900);
        on_event(format!("preparing: {}", step.name));
        let result = run_job_watched(&runners, wt.path(), &step, on_event);
        if !result.ok {
            let tail: String = result.output.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            return Err(format!(
                "the worktree could not be prepared: `{}` failed{}. The change was not attempted.\n{tail}",
                step.name,
                if sandbox.is_some() { " in the sandbox" } else { "" }
            ));
        }
    }

    // The untouched tree first. A check that fails before any change, and
    // fails the same way after it, is the repository's problem and not the
    // change's; holding it against the agent means every attempt fails the
    // same way. A check that fails differently, or newly, counts.
    let mut baseline: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut unfinished: Vec<String> = Vec::new();
    if !jobs.is_empty() {
        on_event("running the checks on the untouched tree first".into());
        for j in &jobs {
            budget.check("while checking the untouched tree")?;
            on_event(format!("checking `{}` on the untouched tree", j.name));
            let result = run_check(&runners, wt.path(), &budget.fit(j), on_event);
            budget.stop.check()?;
            if result.ok {
                on_event(format!("`{}` passes on {} ({})", j.name, job.base, took(result.duration_secs)));
                continue;
            }
            // It does not finish here, before any change: running it again
            // after every round is that long again each time, for nothing
            // it could say about the change.
            if crate::local_ci::runner::timed_out(&result.output) {
                let last = result.output.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with("--- ") && !crate::local_ci::runner::timed_out(l)).unwrap_or("no output").trim();
                on_event(format!(
                    "`{}` does not finish on {} where the checks run (stopped after {}; last line: {}); not run again in this run",
                    j.name,
                    job.base,
                    took(result.duration_secs),
                    last.chars().take(140).collect::<String>()
                ));
                unfinished.push(j.name.clone());
                continue;
            }
            if let Some(why) = machine_failure(&result.output) {
                return Err(machine_failure_message(&j.name, why, &result.output));
            }
            if let Some(program) = missing_program(&result.output) {
                return Err(format!(
                    "the check `{}` needs `{program}`, which is not installed where the checks run{}.\n{}",
                    j.name,
                    if sandbox.is_some() { " (the sandbox)" } else { " (this machine; DevDock uses your login shell's PATH)" },
                    result.output.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
                ));
            }
            let first = result.output.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            on_event(format!(
                "`{}` already fails on {} before any change ({}): {}",
                j.name,
                job.base,
                took(result.duration_secs),
                first.chars().take(140).collect::<String>()
            ));
            baseline.insert(j.name.clone(), normalize_output(&result.output));
        }
    }
    jobs.retain(|j| !unfinished.contains(&j.name));
    let unfinished: Vec<String> = unfinished.into_iter().map(|name| format!("{name}{UNFINISHED}")).collect();
    let mut skipped: Vec<String> = Vec::new();

    let tracked = wt.tracked_files().map_err(|e| e.to_string())?;
    let mut workspace = Workspace::new(wt.path(), tracked.clone(), Access::ReadWrite)?
        .with_write_mode(WriteMode::Live)
        .with_checks(jobs.clone())
        .with_commands(true);
    if let Some(sandbox) = &sandbox {
        workspace = workspace.with_sandbox(sandbox.clone(), runners.clone());
    }
    if let Some(ask) = &job.ask {
        workspace = workspace.with_asker(ask.clone());
    }
    // The repository's MCP servers, started where the checks run. One that
    // cannot start is logged and left out; the run goes on without it.
    let host_launcher = crate::agent::mcp::HostLauncher;
    let sandbox_launcher = sandbox.as_ref().map(|s| crate::sandbox::SandboxRunner(s.clone()));
    let launcher: &dyn crate::agent::mcp::Launcher = match &sandbox_launcher {
        Some(l) => l,
        None => &host_launcher,
    };
    let mut servers = Vec::new();
    for spec in crate::agent::mcp::declared(wt.path()) {
        let name = spec.name.clone();
        match crate::agent::mcp::Server::start(spec, launcher, wt.path()) {
            Ok(server) => {
                on_event(format!(
                    "MCP: {name} started{} with {} tool(s): {}",
                    if sandbox.is_some() { " in the sandbox" } else { "" },
                    server.tools().len(),
                    server.tools().iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
                ));
                servers.push(server);
            }
            Err(e) => on_event(format!("MCP: {name} not started: {e}")),
        }
    }
    if !servers.is_empty() {
        workspace = workspace.with_mcp(servers);
    }
    let base_task = task_text(job.task, job.ask.is_some());
    let mut context = String::from("This is an unattended run on a fresh worktree of the repository. ");
    context.push_str(crate::screenshots::SCREENS_NOTE);
    if !baseline.is_empty() {
        context.push_str(&format!(
            " These checks already fail on {} before any change: {}. If the task is about \
             them, fix them; if not, a failure that is the same as before will not be held \
             against you, but do not make it worse.",
            job.base,
            baseline.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    let rounds = job.rounds.max(1);
    // The advisor: whoever reviews, else the fixer's engine in a fresh
    // session — a second pair of eyes on the same model still helps.
    let advisor = job.reviewer.unwrap_or(engine);
    let mut feedback: Option<String> = None;
    // A reply starts from the conversation before it.
    let mut history: Vec<coding::Turn> = job.task.earlier.clone();
    let mut last_diff: Option<String> = None;
    let mut turns = 0;
    let mut summary;
    let mut checks: Vec<CheckOutcome> = Vec::new();
    let mut reviewed_by: Option<String> = None;
    let mut revises = 0;
    let mut round = 0;
    loop {
        round += 1;
        budget.check(&format!("before round {round}"))?;
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
                context: Some(&context),
                images: &job.task.images,
                history: &history,
                ..coding::Request::new(&task)
            },
            &mut |event: Event| on_event(event.line()),
        )?;
        turns += run.turns;
        summary = run.text;
        history.push(coding::Turn {
            task: feedback.clone().unwrap_or_else(|| "(the task above)".into()),
            summary: summary.clone(),
        });

        // Nothing changed: the agent gave up, or thinks it needs a person.
        // Before believing it, a second agent looks.
        if changed_files(&wt)?.is_empty() {
            if round >= rounds {
                return Err(format!("the agent changed nothing: {}", first_line(&summary)));
            }
            on_event(format!("the agent changed nothing; asking {} what to do", advisor.label()));
            let happened = format!("The agent made no edits. It said:\n{summary}");
            let advice = advise_with(advisor, wt.path(), &tracked, &job.task.brief, &happened, "", on_event)?;
            if !advice.doable {
                return Err(format!(
                    "the agent changed nothing, and {} agreed it needs a person: {}\n\nThe agent said: {}",
                    advisor.label(),
                    advice.advice,
                    first_line(&summary)
                ));
            }
            on_event(format!("advice: {}", first_line(&advice.advice)));
            feedback = Some(format!(
                "You changed nothing and said:\n{summary}\n\nA senior engineer read the \
                 repository and says it is doable without a person:\n{}",
                advice.advice
            ));
            continue;
        }

        // A round that produced exactly the last round's change is not
        // going anywhere; more rounds would only cost.
        let diff = wt.git(&["diff", job.base]).map_err(|e| e.to_string())?;
        if last_diff.as_deref() == Some(diff.as_str()) {
            return Err(format!(
                "no progress: round {round} left the tree exactly as round {} did.\n{}",
                round - 1,
                feedback.as_deref().unwrap_or("")
            ));
        }
        last_diff = Some(diff.clone());

        // The checks, run here. The agent's word that they passed is not
        // what a draft pull request should rest on.
        checks.clear();
        skipped.clone_from(&unfinished);
        let mut failed: Option<String> = None;
        for j in &jobs {
            budget.check(&format!("while verifying round {round}"))?;
            on_event(format!("verifying: {}", j.name));
            let result = run_check(&runners, wt.path(), &budget.fit(j), on_event);
            budget.stop.check()?;
            if !result.ok {
                if let Some(before) = baseline.get(&j.name) {
                    if *before == normalize_output(&result.output) {
                        on_event(format!("{} fails exactly as it did before the change; not counted", j.name));
                        skipped.push(j.name.clone());
                        continue;
                    }
                }
            }
            on_event(format!("{} {} ({})", j.name, if result.ok { "passed" } else { "FAILED" }, took(result.duration_secs)));
            checks.push(CheckOutcome { name: j.name.clone(), ok: result.ok });
            if !result.ok {
                // The machine's fault — a linker killed for memory, a full
                // disk — or a program the check needs that is not there:
                // the environment's, not the change's. Saying so beats
                // another round that fails the same way.
                if let Some(why) = machine_failure(&result.output) {
                    return Err(machine_failure_message(&j.name, why, &result.output));
                }
                if let Some(program) = missing_program(&result.output) {
                    return Err(format!(
                        "the check `{}` needs `{program}`, which is not installed where the checks run{}.\n{}",
                        j.name,
                        if sandbox.is_some() { " (the sandbox)" } else { " (this machine; DevDock uses your login shell's PATH)" },
                        result.output.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
                    ));
                }
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
            // The failure, and a diagnosis of it from a second agent that
            // reads the code rather than only the output.
            on_event(format!("check failed; asking {} what went wrong", advisor.label()));
            let happened = format!("{why}\n\nThe agent's account of the attempt:\n{summary}");
            let advice = advise_with(advisor, wt.path(), &tracked, &job.task.brief, &happened, &diff, on_event)?;
            if !advice.doable {
                return Err(format!(
                    "a check fails, and {} says fixing it needs a person: {}\n{why}",
                    advisor.label(),
                    advice.advice
                ));
            }
            on_event(format!("advice: {}", first_line(&advice.advice)));
            on_event("sending the failure back to the agent".into());
            feedback = Some(format!("{why}\n\nA senior engineer read the code and says:\n{}", advice.advice));
            continue;
        }

        // A second opinion, before anyone else sees it.
        if let Some(reviewer) = job.reviewer {
            on_event(format!("review by {}", reviewer.label()));
            let brief = if skipped.is_empty() {
                job.task.brief.clone()
            } else {
                format!(
                    "{}\n\nNote: these checks fail on the base branch and still fail the same way after the change: {}. Judge whether the task required fixing them.",
                    job.task.brief,
                    skipped.join(", ")
                )
            };
            let verdict = review_with(reviewer, wt.path(), &tracked, &brief, &diff, on_event)?;
            if verdict.approve {
                on_event(format!("approved: {}", first_line(&verdict.feedback)));
                reviewed_by = Some(reviewer.label());
            } else {
                on_event(format!("revise: {}", first_line(&verdict.feedback)));
                revises += 1;
                if round >= rounds {
                    return Err(format!(
                        "after {rounds} round(s) the reviewer still asked for changes:\n{}",
                        verdict.feedback
                    ));
                }
                let mut next = format!("A reviewer read your change and asked for changes:\n{}", verdict.feedback);
                // Twice in a row is a disagreement, not a correction. A third
                // party decides whether the demand can be met at all, before
                // more rounds go the same way.
                if revises >= 2 {
                    on_event(format!("the reviewer asked twice; asking {} to arbitrate", advisor.label()));
                    let happened = format!(
                        "The reviewer has asked for changes {revises} rounds in a row. Its latest \
                         feedback:\n{}\n\nThe agent's account of its latest attempt:\n{summary}\n\n\
                         Decide whether the reviewer's demand is something the agent can and should \
                         do in this repository. Where the ticket's wording and the repository's own \
                         tests conflict, the tests win. If the demand needs a decision no person has \
                         made, say it needs a person.",
                        verdict.feedback
                    );
                    let advice = advise_with(advisor, wt.path(), &tracked, &job.task.brief, &happened, &diff, on_event)?;
                    if !advice.doable {
                        return Err(format!(
                            "the reviewer and the agent could not agree after {round} round(s), and {} says \
                             it needs a person: {}\n\nThe reviewer's last word: {}",
                            advisor.label(),
                            advice.advice,
                            verdict.feedback
                        ));
                    }
                    on_event(format!("advice: {}", first_line(&advice.advice)));
                    next.push_str(&format!("\n\nA senior engineer weighed in:\n{}", advice.advice));
                }
                feedback = Some(next);
                continue;
            }
        }
        break;
    }

    let changes = changed_files(&wt)?;
    for c in &changes {
        on_event(format!("changed {} +{} -{}", c.path, c.added, c.removed));
    }

    // What it looks like, for a change to something with a screen: taken
    // where the checks ran — or in a sandbox started for it when the run
    // had none and the tree needs one — kept outside the repository,
    // never part of the change.
    let shots = crate::screenshots::capture_anywhere(wt.path(), &runners, sandbox.clone(), branch, on_event);

    // Commit and push.
    stage_change(&wt)?;
    let subject = truncate(&job.task.subject(), 72);
    let body = match &job.task.url {
        Some(url) => format!("{}\n\n{url}", summary.trim()),
        None => summary.trim().to_string(),
    };
    wt.commit(&subject, &body, false).map_err(|e| e.to_string())?;
    on_event(format!("committed: {subject}"));
    wt.push_branch(branch, false, job.auth).map_err(|e| format!("push failed: {e}"))?;
    on_event(format!("pushed {branch}"));

    // The pull request.
    let (title, pr_body) =
        pull_request_text(&summary, job.task, &checks, &skipped, &engine.label(), round, reviewed_by.as_deref());
    let pr = publish(&title, &pr_body, branch)?;
    on_event(format!("draft pull request #{} opened", pr.number));

    Ok(Fixed {
        key: job.task.label.clone(),
        branch: branch.to_string(),
        pr,
        summary,
        changes,
        checks,
        turns,
        engine: engine.label(),
        rounds: round,
        reviewed_by,
        skipped,
        screenshots: shots.shots,
        no_screenshots: shots.missed,
    })
}

/// The worktree as a git tree object, everything staged: what a second
/// agent's run is checked against afterwards, and put back to.
fn tree_snapshot(wt: &Repo) -> Result<String, String> {
    wt.stage_all().map_err(|e| e.to_string())?;
    wt.git(&["write-tree"]).map(|t| t.trim().to_string()).map_err(|e| e.to_string())
}

/// Puts the worktree back to `tree`: files it had are restored, files
/// made since are removed.
fn restore_tree(wt: &Repo, tree: &str) -> Result<(), String> {
    wt.git(&["read-tree", "--reset", "-u", tree]).map_err(|e| e.to_string())?;
    wt.git(&["clean", "-fd"]).map(drop).map_err(|e| e.to_string())
}

/// Runs a second agent — a reviewer, an advisor — over the tree and keeps
/// the tree as it was: such an agent may run anything, so it can run the
/// checks and look around without being refused, and whatever it changed
/// is put back afterwards. Its answer counts; its edits do not.
fn with_tree_kept<T>(
    root: &Path,
    who: &str,
    on_event: &mut dyn FnMut(String),
    run: impl FnOnce(&mut dyn FnMut(String)) -> Result<T, String>,
) -> Result<T, String> {
    let wt = Repo::open(root).map_err(|e| e.to_string())?;
    let before = tree_snapshot(&wt)?;
    let result = run(on_event);
    let after = tree_snapshot(&wt)?;
    if after != before {
        on_event(format!("{who} changed the tree; what it said counts, what it changed is put back"));
        restore_tree(&wt, &before)?;
    }
    result
}

/// Reviews the change with whichever engine: the harness over a read-only
/// workspace on the changed tree, or Claude Code or OpenCode over the
/// tree, which is kept as it was.
fn review_with(
    reviewer: &Engine,
    root: &Path,
    tracked: &[String],
    brief: &str,
    diff: &str,
    on_event: &mut dyn FnMut(String),
) -> Result<crate::agent::backlog::Verdict, String> {
    with_tree_kept(root, "the reviewer", on_event, |on_event| match reviewer {
        Engine::Harness(provider) => {
            let mut workspace = Workspace::new(root, tracked.to_vec(), Access::ReadOnly)?;
            crate::agent::backlog::review(provider.as_ref(), &mut workspace, brief, diff, &mut |e| on_event(e.line()))
        }
        Engine::ClaudeCode(config) => {
            let task = crate::agent::backlog::review_task(brief, diff);
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
        Engine::OpenCode(config) => {
            let task = crate::agent::backlog::review_task(brief, diff);
            let run = crate::agent::opencode::run(
                config,
                root,
                crate::agent::opencode::Launch {
                    task: &task,
                    instructions: Some(
                        "You are reviewing a change an unattended coding agent made, before it \
                         becomes a pull request. Read only; run the repository's checks if useful. \
                         Be strict: revise unless you would merge it. Answer with JSON only: \
                         {\"verdict\": \"approve\" | \"revise\", \"feedback\": \"…\"}",
                    ),
                    permissions: crate::agent::opencode::Permissions::ReadOnly,
                    files: &[],
                    resume: None,
                    collect_edits: false,
                },
                &mut |e| on_event(e.line()),
            )?;
            Ok(crate::agent::backlog::parse_verdict(&run.text))
        }
    })
}

/// Asks a second agent what to do about a round that did not get through:
/// the harness over a read-only workspace, or Claude Code or OpenCode over
/// the tree, which is kept as it was.
fn advise_with(
    advisor: &Engine,
    root: &Path,
    tracked: &[String],
    brief: &str,
    happened: &str,
    diff: &str,
    on_event: &mut dyn FnMut(String),
) -> Result<crate::agent::backlog::Advice, String> {
    with_tree_kept(root, "the advisor", on_event, |on_event| match advisor {
        Engine::Harness(provider) => {
            let mut workspace = Workspace::new(root, tracked.to_vec(), Access::ReadOnly)?;
            crate::agent::backlog::advise(provider.as_ref(), &mut workspace, brief, happened, diff, &mut |e| on_event(e.line()))
        }
        Engine::ClaudeCode(config) => {
            let task = crate::agent::backlog::advise_task(brief, happened, diff);
            let run = crate::agent::claude_code::run_readonly(
                config,
                root,
                &task,
                Some(
                    "You are the senior engineer pairing with an unattended coding agent whose \
                     last attempt did not get through. Read the repository, work out what went \
                     wrong, and say exactly what to do next. Answer with JSON only: \
                     {\"doable\": true | false, \"advice\": \"…\"}",
                ),
                &mut |e| on_event(e.line()),
            )?;
            Ok(crate::agent::backlog::parse_advice(&run.text))
        }
        Engine::OpenCode(config) => {
            let task = crate::agent::backlog::advise_task(brief, happened, diff);
            let run = crate::agent::opencode::run(
                config,
                root,
                crate::agent::opencode::Launch {
                    task: &task,
                    instructions: Some(
                        "You are the senior engineer pairing with an unattended coding agent whose \
                         last attempt did not get through. Read the repository, work out what went \
                         wrong, and say exactly what to do next. Answer with JSON only: \
                         {\"doable\": true | false, \"advice\": \"…\"}",
                    ),
                    permissions: crate::agent::opencode::Permissions::ReadOnly,
                    files: &[],
                    resume: None,
                    collect_edits: false,
                },
                &mut |e| on_event(e.line()),
            )?;
            Ok(crate::agent::backlog::parse_advice(&run.text))
        }
    })
}

/// What the worktree has changed against its commit, per git, so the
/// report is of the tree and not of what one engine remembers doing.
fn changed_files(wt: &Repo) -> Result<Vec<ChangedFile>, String> {
    stage_change(wt)?;
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

/// Stages everything the run changed, less what the checks left behind.
fn stage_change(wt: &Repo) -> Result<(), String> {
    wt.stage_all().map_err(|e| e.to_string())?;
    unstage_artifacts(wt)
}

/// Build and cache output that running the checks leaves behind —
/// `__pycache__`, `target/`, `node_modules/`, `.dart_tool/` — in a
/// repository whose `.gitignore` does not cover it. `git add -A` would put
/// it in the pull request; a reviewer would rightly send that back. Only
/// files new since the commit are dropped: an artifact the repository
/// tracks on purpose stays tracked.
fn unstage_artifacts(wt: &Repo) -> Result<(), String> {
    const ARTIFACT_DIRS: &[&str] = &[
        ".devdock", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", "target", "node_modules",
        ".dart_tool", "build", "dist", ".gradle", ".next", ".nuxt", "coverage", ".coverage",
        ".tox", ".venv", "venv", "Pods", "DerivedData", ".idea", ".vscode",
    ];
    const ARTIFACT_EXTENSIONS: &[&str] = &[".pyc", ".pyo", ".class", ".o", ".so", ".dylib", ".dll", ".log"];
    // Generated by a run, and lockfiles a dependency fetch writes where the
    // repository did not have one: not the change's to add.
    const GENERATED: &[&str] = &["devdock_smoke_test.dart", "devdock_smoke.png", "pubspec.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "Gemfile.lock"];
    let added = wt.git(&["diff", "--cached", "--name-only", "--diff-filter=A"]).map_err(|e| e.to_string())?;
    let artifacts: Vec<&str> = added
        .lines()
        .filter(|path| {
            path.split('/').any(|part| ARTIFACT_DIRS.contains(&part))
                || ARTIFACT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
                || GENERATED.iter().any(|g| path.ends_with(g))
                || is_devdock_cargo_config(wt, path)
        })
        .collect();
    if artifacts.is_empty() {
        return Ok(());
    }
    let mut args = vec!["rm", "--cached", "-q", "--"];
    args.extend(artifacts.iter().copied());
    wt.git(&args).map_err(|e| e.to_string())?;
    Ok(())
}

/// Commits whatever a failed run left in its worktree, as work in
/// progress on its branch, and says so. `None` when there was nothing.
fn keep_attempt(dir: &Path, task: &Task, why: &str) -> Option<String> {
    let wt = Repo::open(dir).ok()?;
    let changes = changed_files(&wt).ok()?;
    if changes.is_empty() {
        return None;
    }
    let subject = format!("{WIP_PREFIX}{} (not accepted)", truncate(&task.subject(), 50));
    let body = format!("The coding agent's attempt, kept for a person to finish.\n\nNot accepted because:\n{}", first_line(why));
    wt.commit(&subject, &body, false).ok()?;
    let branch = wt.git(&["rev-parse", "--abbrev-ref", "HEAD"]).ok()?.trim().to_string();
    Some(format!(
        "the attempt is kept on branch {branch} ({} file(s), not pushed): check it out to finish it, or delete the branch",
        changes.len()
    ))
}

/// A check's output with what varies between identical runs taken out —
/// durations, blank lines, surrounding whitespace — so two failures can be
/// told to be the same failure.
fn normalize_output(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The program a shell said it could not find, from a check's output:
/// `sh: 1: dart: not found`, `bash: flutter: command not found`.
pub fn missing_program(output: &str) -> Option<String> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_suffix(": not found").or_else(|| line.strip_suffix(": command not found")) {
            let program = rest.rsplit(':').next().unwrap_or(rest).trim();
            if !program.is_empty() && !program.contains(' ') {
                return Some(program.to_string());
            }
        }
    }
    None
}

/// The whole run's time: however many checks a repository has and however
/// long each may take, a run ends within this, its attempt kept. Each
/// check's own timeout is cut to what is left, so the last one cannot
/// carry the run past it either.
struct Budget {
    started: std::time::Instant,
    limit: std::time::Duration,
    /// The run's kill switch, looked at wherever the time is.
    stop: crate::cancel::Token,
}

impl Budget {
    /// Three hours, or `DEVDOCK_RUN_MINUTES`.
    fn new(root: &Path) -> Self {
        crate::cancel::reset(root);
        let minutes = std::env::var("DEVDOCK_RUN_MINUTES").ok().and_then(|v| v.parse::<u64>().ok()).filter(|m| *m > 0).unwrap_or(180);
        Self { started: std::time::Instant::now(), limit: std::time::Duration::from_secs(minutes * 60), stop: crate::cancel::token(root) }
    }

    fn left(&self) -> std::time::Duration {
        self.limit.saturating_sub(self.started.elapsed())
    }

    /// An error when the time is spent, saying at what.
    fn check(&self, at: &str) -> Result<(), String> {
        self.stop.check()?;
        if self.left().is_zero() {
            return Err(format!(
                "the run reached its limit of {} minutes {at} and was stopped; what it had is kept. \
                 A repository whose checks take this long wants a `.git-manage-ci.toml` naming only the ones that matter.",
                self.limit.as_secs() / 60
            ));
        }
        Ok(())
    }

    /// `j` with its timeout cut to the time left.
    fn fit(&self, j: &crate::local_ci::Job) -> crate::local_ci::Job {
        let mut j = j.clone();
        let own = j.timeout_secs.unwrap_or(crate::local_ci::DEFAULT_TIMEOUT_SECS);
        j.timeout_secs = Some(own.min(self.left().as_secs().max(60)));
        j
    }
}

/// How the commit that keeps a failed run's attempt starts.
const WIP_PREFIX: &str = "WIP: ";

/// How a skipped check that never finished on the base branch is told
/// from one that failed there.
const UNFINISHED: &str = " (did not finish)";

/// A check that failed because of the machine it ran on, not the code:
/// a linker or compiler killed for memory, a full disk. What to say.
pub fn machine_failure(output: &str) -> Option<&'static str> {
    if output.contains("No space left on device") {
        return Some("the disk is full");
    }
    if output.contains("signal: 9") || output.contains("SIGKILL") || output.lines().any(|l| l.trim() == "Killed") {
        return Some("a process was killed, which is the machine running out of memory");
    }
    if output.contains("linking with `cc` failed") || output.contains("linker command failed") {
        return Some("the linker failed, which on a busy machine is usually memory");
    }
    None
}

/// The lines of a build's output that say what the machine did: the
/// linker's and compiler's own words, which cargo prints as notes above
/// its summary and the summary's last lines hide.
fn machine_failure_detail(output: &str) -> String {
    let telling: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("= note:") || l.starts_with("ld:") || l.starts_with("clang:") || l.contains("No space left") || l.contains("signal: 9") || l.contains("Killed")
        })
        .filter(|l| !l.contains("run with `RUST_BACKTRACE"))
        .take(15)
        .collect();
    if telling.is_empty() {
        output.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    } else {
        telling.join("\n")
    }
}

fn machine_failure_message(check: &str, why: &str, output: &str) -> String {
    format!(
        "the check `{check}` failed because of this machine, not the change: {why}. It was run twice, \
         the second time one build job at a time. Close what you can and run the task again; \
         the attempt is kept.\n{}",
        machine_failure_detail(output)
    )
}

/// Runs a check, and a check that fails the machine's way — not the
/// code's — once more after a pause, one build job at a time: a link
/// that was one of several may fit in memory on its own.
fn run_check(runners: &crate::local_ci::runner::RunnerRegistry, root: &Path, j: &crate::local_ci::Job, on_event: &mut dyn FnMut(String)) -> crate::local_ci::JobResult {
    let result = run_job_watched(runners, root, j, on_event);
    if result.ok {
        return result;
    }
    let Some(why) = machine_failure(&result.output) else { return result };
    on_event(format!("{} failed the machine's way ({why}); waiting, then running it once more, one build job at a time", j.name));
    std::thread::sleep(std::time::Duration::from_secs(if cfg!(test) { 0 } else { 20 }));
    let mut gently = j.clone();
    for (k, v) in [("CARGO_BUILD_JOBS", "1"), ("MAKEFLAGS", "-j1"), ("GOFLAGS", "-p=1")] {
        gently.env.entry(k.into()).or_insert_with(|| v.into());
    }
    run_job_watched(runners, root, &gently, on_event)
}

/// Runs a job and, while it runs, says so every five minutes: a test
/// suite that takes half an hour in a small machine is a line every so
/// often, not a card that seems stuck.
fn run_job_watched(runners: &crate::local_ci::runner::RunnerRegistry, root: &Path, j: &crate::local_ci::Job, on_event: &mut dyn FnMut(String)) -> crate::local_ci::JobResult {
    const EVERY: u64 = 300;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = tx.send(crate::local_ci::run_job_with(runners, root, j));
        });
        let started = std::time::Instant::now();
        let mut next = EVERY;
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let secs = started.elapsed().as_secs();
                    if secs >= next {
                        on_event(format!("`{}` still running, {} min in", j.name, secs / 60));
                        next += EVERY;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break crate::local_ci::JobResult {
                        name: j.name.clone(),
                        ok: false,
                        output: "the check's thread ended without a result".into(),
                        duration_secs: started.elapsed().as_secs_f32(),
                    }
                }
            }
        }
    })
}

/// A duration for a log line: `4:12`, or `0:03`.
fn took(secs: f32) -> String {
    let s = secs.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The marker in a cargo config DevDock wrote, so it is recognised and
/// never staged.
const CARGO_CONFIG_MARK: &str = "# Written by DevDock for this worktree; not part of the change.";

/// Points a Rust worktree's cargo at the repository's own `target/`, so an
/// attempt does not build every dependency from nothing — ten minutes a
/// round for an app like this one, three rounds for a colour — but reuses
/// what the repository has already built. Written only where the
/// worktree has no cargo config of its own; the sandbox has its own
/// target directory and needs none of this.
fn share_cargo_target(wt: &Path, repo: &Path) -> Option<String> {
    if !wt.join("Cargo.toml").exists() {
        return None;
    }
    let config = wt.join(".cargo/config.toml");
    if config.exists() || wt.join(".cargo/config").exists() {
        return None;
    }
    let target = repo.join("target");
    let text = format!("{CARGO_CONFIG_MARK}\n[build]\ntarget-dir = {:?}\n", target.display().to_string());
    std::fs::create_dir_all(wt.join(".cargo")).ok()?;
    std::fs::write(&config, text).ok()?;
    Some(format!("cargo builds into {}, shared with the repository", target.display()))
}

/// Whether a staged `.cargo/config.toml` is the one DevDock wrote.
fn is_devdock_cargo_config(wt: &Repo, path: &str) -> bool {
    path == ".cargo/config.toml" && std::fs::read_to_string(wt.path().join(path)).is_ok_and(|t| t.starts_with(CARGO_CONFIG_MARK))
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

    /// A reviewer that edits, deletes and creates: after it, the tree is
    /// exactly what the fixer left, and its answer still comes back.
    #[test]
    fn a_second_agents_edits_are_put_back() {
        let (_tmp, repo) = setup("true");
        let root = repo.path().to_path_buf();
        fs::write(root.join("lib.py"), "def total(xs):\n    return sum(xs)\n").unwrap();
        fs::write(root.join("new.py"), "x = 1\n").unwrap();
        let mut log = Vec::new();
        let got = with_tree_kept(&root, "the reviewer", &mut |l| log.push(l), |_| {
            fs::write(root.join("lib.py"), "broken").unwrap();
            fs::remove_file(root.join("new.py")).unwrap();
            fs::create_dir_all(root.join("notes")).unwrap();
            fs::write(root.join("notes/junk.txt"), "reviewer was here").unwrap();
            Ok::<_, String>("approve")
        })
        .unwrap();
        assert_eq!(got, "approve");
        assert_eq!(fs::read_to_string(root.join("lib.py")).unwrap(), "def total(xs):\n    return sum(xs)\n");
        assert_eq!(fs::read_to_string(root.join("new.py")).unwrap(), "x = 1\n");
        assert!(!root.join("notes").exists(), "what the reviewer made is gone");
        assert!(log.iter().any(|l| l.starts_with("the reviewer changed the tree")), "{log:?}");

        // One that only reads leaves no line.
        let mut log = Vec::new();
        with_tree_kept(&root, "the reviewer", &mut |l| log.push(l), |_| Ok::<_, String>(())).unwrap();
        assert!(log.is_empty(), "{log:?}");
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

    fn task() -> Task {
        Task::from_issue(&issue(), None)
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
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
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
        assert_eq!(fixed.engine, "DevDock harness · scripted");
        assert!(log[0].starts_with("devdock 0."), "the build comes first: {}", log[0]);
        assert_eq!(log.get(1).map(String::as_str), Some("branch fix/abc-7-total-is-off-by-one from main"));
        assert!(log.iter().any(|l| l == "engine: DevDock harness · scripted"), "{log:?}");

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
    fn a_failure_of_the_machine_ends_the_run_without_spending_rounds() {
        assert_eq!(machine_failure("error: linking with `cc` failed: exit status: 1\n  |\n  = note: clang: error: linker command failed"), Some("the linker failed, which on a busy machine is usually memory"));
        assert_eq!(machine_failure("error: could not compile `x` (lib test)\nCaused by: process didn't exit successfully: `rustc …` (signal: 9, SIGKILL: kill)"), Some("a process was killed, which is the machine running out of memory"));
        assert_eq!(machine_failure("write error: No space left on device"), Some("the disk is full"));
        assert_eq!(machine_failure("error: test failed\nassertion `left == right`"), None);

        // A check that fails the machine's way both times: the run ends at
        // once with the reason, no round spent, no advisor asked.
        let (_tmp, repo) = setup(r#"sh -c \"echo 'clang: error: linker command failed with exit code 1' >&2; exit 1\""#);
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_err();
        assert!(err.contains("because of this machine, not the change") && err.contains("the linker failed"), "{err}");
        assert!(err.contains("clang: error: linker command failed"), "the linker's own line is in the message: {err}");
        assert!(log.iter().any(|l| l.contains("one build job at a time")), "{log:?}");
        assert_eq!(
            machine_failure_detail("   Compiling x\nerror: linking with `cc` failed: exit status: 1\n  |\n  = note: some arguments are omitted\n  = note: ld: out of memory\n\nerror: could not compile `x`\nwarning: build failed"),
            "= note: some arguments are omitted\n= note: ld: out of memory"
        );
        assert!(!log.iter().any(|l| l.starts_with("round 1")), "no round was spent: {log:?}");
    }

    #[test]
    fn a_rust_worktree_builds_into_the_repositorys_target() {
        let (_tmp, repo) = setup("true");
        let wt = tempfile::tempdir().unwrap();
        assert_eq!(share_cargo_target(wt.path(), repo.path()), None, "not a Rust tree");
        fs::write(wt.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let line = share_cargo_target(wt.path(), repo.path()).unwrap();
        assert!(line.contains("shared with the repository"), "{line}");
        let text = fs::read_to_string(wt.path().join(".cargo/config.toml")).unwrap();
        assert!(text.starts_with(CARGO_CONFIG_MARK) && text.contains("target-dir") && text.contains("target\""), "{text}");
        // A tree with its own config keeps it.
        fs::write(wt.path().join(".cargo/config.toml"), "[build]\nrustflags = []\n").unwrap();
        assert_eq!(share_cargo_target(wt.path(), repo.path()), None);
        assert_eq!(fs::read_to_string(wt.path().join(".cargo/config.toml")).unwrap(), "[build]\nrustflags = []\n");

        // The one DevDock wrote is never part of the change; one the agent wrote is.
        fs::write(repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        share_cargo_target(repo.path(), repo.path()).unwrap();
        stage_change(&repo).unwrap();
        let staged = repo.git(&["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("Cargo.toml") && !staged.contains(".cargo/config.toml"), "{staged}");
        fs::write(repo.path().join(".cargo/config.toml"), "[build]\nrustflags = [\"-D\", \"warnings\"]\n").unwrap();
        stage_change(&repo).unwrap();
        let staged = repo.git(&["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains(".cargo/config.toml"), "{staged}");
    }

    #[test]
    fn a_missing_program_fails_the_run_as_the_environments_fault() {
        assert_eq!(missing_program("--- stderr ---\nsh: 1: dart: not found\n").as_deref(), Some("dart"));
        assert_eq!(missing_program("bash: flutter: command not found").as_deref(), Some("flutter"));
        assert_eq!(missing_program("error: test failed\nassertion `left == right`").as_deref(), None);

        let (_tmp, repo) = setup("no-such-program-xyz analyze");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_err();
        assert!(err.contains("needs `no-such-program-xyz`, which is not installed"), "{err}");
        assert!(!log.iter().any(|l| l == "round 2 of 3"), "no round was spent on it: {log:?}");
    }

    #[test]
    fn a_worktree_comes_with_its_submodules_and_its_dependencies_fetched() {
        let (tmp, repo) = setup("test -f schemas/schema.txt && test -f node_modules/.devdock-prepared");
        // A submodule, and a package.json whose install step the run must do.
        let sub = tmp.path().join("schemas-src");
        fs::create_dir_all(&sub).unwrap();
        sh(&sub, &["init", "-q", "-b", "main"]);
        sh(&sub, &["config", "user.email", "t@t.io"]);
        sh(&sub, &["config", "user.name", "T"]);
        fs::write(sub.join("schema.txt"), "schema\n").unwrap();
        sh(&sub, &["add", "-A"]);
        sh(&sub, &["commit", "-q", "-m", "schema"]);
        // A local-path submodule needs git's file transport allowed, for
        // this add and for the worktree's update: the environment reaches
        // both.
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "protocol.file.allow");
        std::env::set_var("GIT_CONFIG_VALUE_0", "always");
        sh(repo.path(), &["submodule", "add", "-q", sub.to_str().unwrap(), "schemas"]);
        // "npm install" here is a script that leaves a marker, so the test
        // needs no Node.
        fs::create_dir_all(repo.path().join("bin")).unwrap();
        fs::write(repo.path().join("bin/npm"), "#!/bin/sh\nmkdir -p node_modules && touch node_modules/.devdock-prepared\n").unwrap();
        fs::write(repo.path().join("package.json"), "{\"name\": \"x\"}\n").unwrap();
        fs::write(repo.path().join(".gitignore"), "node_modules/\n").unwrap();
        sh(repo.path(), &["add", "-A"]);
        sh(repo.path(), &["commit", "-q", "-m", "submodule and package"]);
        sh(repo.path(), &["push", "-q", "origin", "main"]);
        let bin = repo.path().join("bin");
        std::fs::set_permissions(bin.join("npm"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        // The check and the prepare step both need the fake npm on PATH.
        std::env::set_var("PATH", &path);

        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(log.iter().any(|l| l == "preparing: npm install"), "{log:?}");
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        let paths: Vec<&str> = fixed.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["lib.py"], "neither the submodule nor node_modules is part of the change");
    }

    /// A reply to a run that did not get through continues it: the same
    /// branch, the attempt's changes still there and its work-in-progress
    /// commit taken back off, the conversation before it in the prompt —
    /// and one real commit at the end, named for the work, not the reply.
    #[test]
    fn a_reply_continues_a_kept_attempt_on_its_branch() {
        // Passes only with the fix and a second file the first run never writes.
        // It prints the file when it fails, so a failure after the change
        // is not the same failure as before it.
        let (_tmp, repo) = setup("test -f notes.txt && grep -q 'return sum(xs)$' lib.py || { cat lib.py; exit 1; }");
        let prompt = "fix total() and note it\n\nIt adds one too many.";
        let first = Task::from_prompt(prompt);
        let branch = first.branch.clone();
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let err = fix(&repo, &engine, &Job { task: &first, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {})
            .unwrap_err();
        assert_eq!(crate::app::backlog::kept_branch(&err).as_deref(), Some(branch.as_str()), "{err}");
        assert!(repo.git(&["log", "-1", "--format=%s", &branch]).unwrap().starts_with(WIP_PREFIX));

        // Without a reply, the branch is in the way — and the error says what to do.
        let again = fix(&repo, &Engine::Harness(Box::new(fixing_provider())), &Job { task: &first, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {})
            .unwrap_err();
        assert!(again.contains("already exists") && again.contains("Reply on its card"), "{again}");

        // The reply: the fix is already there; it only adds the note.
        let reply = Task::reply_to(prompt, &branch, &err, "add notes.txt saying what changed");
        assert_eq!(reply.title, first.title, "named for the work, not for the reply");
        assert!(reply.brief.contains("add notes.txt") && reply.earlier[0].summary.contains("did not get through"));
        let noting = Scripted(RefCell::new(vec![
            Reply {
                text: String::new(),
                calls: vec![ToolCall { id: "1".into(), name: "write_file".into(), input: serde_json::json!({"path": "notes.txt", "content": "total() no longer adds one\n"}) }],
                ..Default::default()
            },
            Reply { text: String::new(), calls: vec![ToolCall { id: "2".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "- notes.txt: says what changed\n\nVerified: tests".into(), ..Default::default() },
        ]));
        let mut log = Vec::new();
        let fixed = fix(&repo, &Engine::Harness(Box::new(noting)), &Job { task: &reply, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |l| log.push(l))
            .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(log.iter().any(|l| l == &format!("continuing on {branch}, with what the last attempt left")), "{log:#?}");
        let paths: Vec<&str> = fixed.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["lib.py", "notes.txt"], "the attempt's change and the reply's, together");
        let subjects = repo.git(&["log", "--format=%s", &format!("main..{branch}")]).unwrap();
        assert_eq!(subjects.lines().count(), 1, "one real commit, the WIP one gone: {subjects}");
        assert!(!subjects.contains("WIP") && subjects.contains("fix total() and note it"), "{subjects}");
    }

    /// The kill switch: a run in the middle of a check that would take
    /// half a minute ends within a second or two of being stopped, says it
    /// was stopped rather than that it failed, and spends no round on it.
    #[test]
    fn a_stopped_run_ends_where_it_is() {
        let (_tmp, repo) = setup("sleep 30");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log: Vec<String> = Vec::new();
        let started = std::time::Instant::now();
        let mut worktree: Option<std::path::PathBuf> = None;
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| {
                if let Some(path) = line.strip_prefix("worktree ") {
                    worktree = Some(path.into());
                }
                // Stop is pressed a moment into the first check.
                if line.starts_with("checking `tests`") {
                    let root = worktree.clone().expect("the worktree is named before any check");
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        crate::cancel::stop(&root);
                    });
                }
                log.push(line);
            },
        )
        .unwrap_err();
        assert!(crate::cancel::was_stopped(&err), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(!log.iter().any(|l| l.starts_with("round ")), "no round was started: {log:#?}");
        assert!(!crate::cancel::token(&worktree.unwrap()).is_stopped(), "the next run here starts clean");
    }

    #[test]
    fn a_run_has_a_limit_and_checks_are_cut_to_what_is_left() {
        let budget = Budget { started: std::time::Instant::now(), limit: std::time::Duration::from_secs(600), stop: Default::default() };
        assert!(budget.check("anywhere").is_ok());
        let j = crate::local_ci::Job { name: "t".into(), ..Default::default() };
        let fitted = budget.fit(&j).timeout_secs.unwrap();
        assert!((590..=600).contains(&fitted), "the default half hour is cut to the ten minutes left: {fitted}");
        let short = crate::local_ci::Job { timeout_secs: Some(30), ..j.clone() };
        assert_eq!(budget.fit(&short).timeout_secs, Some(30), "a shorter timeout of its own stays");

        let spent = Budget { started: std::time::Instant::now() - std::time::Duration::from_secs(61), limit: std::time::Duration::from_secs(60), stop: Default::default() };
        let err = spent.check("before round 2").unwrap_err();
        assert!(err.contains("limit of 1 minutes before round 2") && err.contains("kept"), "{err}");
        assert_eq!(spent.fit(&j).timeout_secs, Some(60), "never less than a minute");
    }

    /// A check that hangs on the untouched tree — a dependency fetch that
    /// never resolves — is stopped once, named, and not run again: the run
    /// goes on with the checks that do finish.
    #[test]
    fn a_check_that_never_finishes_on_the_base_is_run_once() {
        let (_tmp, repo) = setup("true");
        fs::write(
            repo.path().join(".git-manage-ci.toml"),
            "[[job]]\nname = \"analyze\"\ntimeout_secs = 1\ncommands = [\"echo 'Resolving dependencies...' && sleep 30\"]\n\n[[job]]\nname = \"tests\"\ncommands = [\"grep -q 'return sum(xs)$' lib.py\"]\n",
        )
        .unwrap();
        sh(repo.path(), &["commit", "-q", "-am", "a check that hangs"]);
        sh(repo.path(), &["push", "-q", "origin", "main"]);
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let started = std::time::Instant::now();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "the hang was paid for once: {:?}", started.elapsed());
        assert!(log.iter().any(|l| l.starts_with("`analyze` does not finish on main") && l.contains("Resolving dependencies...") && l.ends_with("not run again in this run")), "{log:#?}");
        assert!(!log.iter().any(|l| l == "verifying: analyze"), "{log:#?}");
        assert_eq!(fixed.skipped, ["analyze (did not finish)"]);
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
    }

    #[test]
    fn a_check_that_already_fails_on_the_base_is_not_held_against_the_change() {
        // "lint" fails on main as it is; "tests" passes once the fix is in.
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py");
        fs::write(
            repo.path().join(".git-manage-ci.toml"),
            "[[job]]\nname = \"lint\"\ncommands = [\"echo 'lib.py:1: style: pre-existing problem' && false\"]\n\n[[job]]\nname = \"tests\"\ncommands = [\"grep -q 'return sum(xs)$' lib.py\"]\n",
        )
        .unwrap();
        sh(repo.path(), &["commit", "-q", "-am", "a failing lint on main"]);
        sh(repo.path(), &["push", "-q", "origin", "main"]);
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(log.iter().any(|l| l.starts_with("`lint` already fails on main before any change (") && l.contains("): lib.py:1: style")), "{log:?}");
        assert!(log.iter().any(|l| l == "lint fails exactly as it did before the change; not counted"), "{log:?}");
        assert_eq!(fixed.skipped, ["lint"]);
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        assert!(fixed.pr.number == 42);
    }

    #[test]
    fn a_declared_mcp_servers_tool_is_used_by_the_fixer() {
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py");
        fs::write(repo.path().join("adder_mcp.py"), crate::agent::mcp::tests::ADDER).unwrap();
        fs::write(repo.path().join(".mcp.json"), r#"{"mcpServers": {"adder": {"command": "python3", "args": ["adder_mcp.py"]}}}"#).unwrap();
        sh(repo.path(), &["add", "-A"]);
        sh(repo.path(), &["commit", "-q", "-m", "an mcp server"]);
        sh(repo.path(), &["push", "-q", "origin", "main"]);
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "m".into(), name: "mcp__adder__add".into(), input: serde_json::json!({"a": 40, "b": 2}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "fixed; the adder said 42".into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(log.iter().any(|l| l == "MCP: adder started with 1 tool(s): add"), "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("· adder: add")), "{log:?}");
        assert_eq!(fixed.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["lib.py"]);
    }

    #[test]
    fn what_the_checks_leave_behind_is_not_part_of_the_change() {
        // The check compiles lib.py, which writes __pycache__/… next to it,
        // and the repository has no .gitignore.
        let (_tmp, repo) = setup("mkdir -p __pycache__ && echo x > __pycache__/lib.cpython-312.pyc && echo y > build.log && grep -q 'return sum(xs)$' lib.py");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |_| {},
        )
        .unwrap();
        let paths: Vec<&str> = fixed.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["lib.py"], "artifacts were swept into the change");
        let tree = repo.git(&["ls-tree", "-r", "--name-only", &fixed.branch]).unwrap();
        assert!(!tree.contains("pycache") && !tree.contains("build.log"), "{tree}");
    }

    #[test]
    fn a_change_that_fails_the_check_is_kept_on_the_branch_unpushed() {
        // The check wants something the fix does not do.
        let (_tmp, repo) = setup("cat lib.py && grep -q 'return 0$' lib.py");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        // The agent's own run_check will fail too; it answers anyway.
        let mut log = Vec::new();
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_err();
        assert!(err.contains("still fails a check"), "{err}");
        assert!(err.contains("the attempt is kept on branch fix/abc-7-total-is-off-by-one (1 file(s), not pushed)"), "{err}");
        // The attempt is a commit on the branch, here and not on the
        // remote; the worktree is gone; main is untouched.
        let subject = repo.log(1, Some("fix/abc-7-total-is-off-by-one")).unwrap()[0].subject.clone();
        assert_eq!(subject, "WIP: ABC-7: total() is off by one (not accepted)");
        let lib = repo.git(&["show", "fix/abc-7-total-is-off-by-one:lib.py"]).unwrap();
        assert!(lib.contains("return sum(xs)\n"), "{lib}");
        let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
        assert!(!remote.contains("fix/"), "{remote}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
        assert_eq!(fs::read_to_string(repo.path().join("lib.py")).unwrap(), "def total(xs):\n    return sum(xs) + 1\n");
        assert!(log.iter().any(|l| l.starts_with("the attempt is kept on branch")), "{log:?}");

        // Running the same ticket again is refused until that branch is
        // dealt with, and the refusal says how.
        let err = fix(&repo, &engine, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("git branch -D fix/abc-7-total-is-off-by-one"), "{err}");
    }

    #[test]
    fn an_agent_that_changes_nothing_is_a_failure_not_a_pull_request() {
        let (_tmp, repo) = setup("true");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![Reply { text: "This needs a product decision.".into(), ..Default::default() }]))));
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
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
        let (_tmp, repo) = setup("cat lib.py && grep -q 'return 0$' lib.py");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "first try".into(), ..Default::default() },
            // The same engine, as the advisor, in a fresh session.
            Reply { text: r#"{"doable": true, "advice": "The check greps for `return 0`; make total() return 0."}"#.into(), ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "2".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "return sum(xs)", "new_text": "return 0"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "second try".into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.rounds, 2);
        assert_eq!(fixed.summary, "second try");
        assert!(log.iter().any(|l| l == "sending the failure back to the agent"), "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("check failed; asking DevDock harness")), "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("advice: The check greps")), "{log:?}");
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
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: Some(&reviewer), ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.rounds, 2);
        assert_eq!(fixed.reviewed_by.as_deref(), Some("DevDock harness · scripted"));
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
        let err = fix(&repo, &fixer, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: Some(&reviewer), ask: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("reviewer still asked for changes"), "{err}");
        assert!(err.contains("kept on branch"), "the attempt survives for a person: {err}");
        let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
        assert!(!remote.contains("fix/"), "{remote}");
    }

    #[test]
    fn an_agent_that_gives_up_is_sent_back_with_advice_and_can_finish() {
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            // Round 1: nothing done, "needs a decision".
            Reply { text: "This needs a product decision about rounding.".into(), ..Default::default() },
            // The advisor (same engine, fresh session) disagrees.
            Reply { text: r#"{"doable": true, "advice": "No decision is needed: total() adds a stray 1 in lib.py line 2. Remove it and run the tests."}"#.into(), ..Default::default() },
            // Round 2, with the advice.
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "Removed the stray + 1.".into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.rounds, 2);
        assert_eq!(fixed.summary, "Removed the stray + 1.");
        assert!(log.iter().any(|l| l == "the agent changed nothing; asking DevDock harness · scripted what to do"), "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("advice: No decision is needed")), "{log:?}");

        // When the advisor agrees a person is needed, that is the end, and
        // both opinions are in the reason.
        let (_tmp, repo) = setup("true");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: "Needs a designer.".into(), ..Default::default() },
            Reply { text: r#"{"doable": false, "advice": "The ticket asks for a visual choice nobody has made."}"#.into(), ..Default::default() },
        ]))));
        let err = fix(&repo, &engine, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 3, reviewer: None, ask: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("agreed it needs a person: The ticket asks for a visual choice"), "{err}");
        assert!(err.contains("The agent said: Needs a designer."), "{err}");
        assert!(!repo.branches().unwrap().local.iter().any(|b| b.name.starts_with("fix/")));
    }

    #[test]
    fn a_round_that_repeats_the_last_change_stops_the_run() {
        // The check can never pass; the agent makes the same edit twice.
        let (_tmp, repo) = setup("cat lib.py && grep -q 'return 0$' lib.py");
        let edit = || Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() };
        let check = || Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() };
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            edit(), check(), Reply { text: "try 1".into(), ..Default::default() },
            Reply { text: r#"{"doable": true, "advice": "return 0"}"#.into(), ..Default::default() },
            // Round 2: the same edit, which is now a no-op.
            edit(), check(), Reply { text: "try 2".into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let err = fix(&repo, &engine, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 5, reviewer: None, ask: None }, &fake_pr, &mut |line| log.push(line)).unwrap_err();
        assert!(err.starts_with("no progress: round 2 left the tree exactly as round 1 did"), "{err}\n{log:#?}");
        assert!(err.contains("kept on branch"), "{err}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    #[test]
    fn a_reviewer_that_keeps_objecting_is_arbitrated() {
        let (_tmp, repo) = setup("true");
        let edit = |id: &str, old: &str, new: &str| Reply { text: String::new(), calls: vec![ToolCall { id: id.into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": old, "new_text": new}) }], ..Default::default() };
        let check = || Reply { text: String::new(), calls: vec![ToolCall { id: "c".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() };
        let fixer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            edit("1", "sum(xs) + 1", "sum(xs)"), check(), Reply { text: "fixed".into(), ..Default::default() },
            edit("2", "return sum(xs)", "return sum(xs)  # per ticket"), check(), Reply { text: "fixed, noted".into(), ..Default::default() },
        ]))));
        // The reviewer wants a product decision the ticket did not make;
        // as the advisor, it is asked to arbitrate and says so.
        let reviewer = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: r#"{"verdict": "revise", "feedback": "The ticket says to ask the owner about empty lists first."}"#.into(), ..Default::default() },
            Reply { text: r#"{"verdict": "revise", "feedback": "Still no owner decision on empty lists."}"#.into(), ..Default::default() },
            Reply { text: r#"{"doable": false, "advice": "The empty-list behaviour is a product decision nobody has made."}"#.into(), ..Default::default() },
        ]))));
        let mut log = Vec::new();
        let err = fix(&repo, &fixer, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 6, reviewer: Some(&reviewer), ask: None }, &fake_pr, &mut |line| log.push(line)).unwrap_err();
        assert!(err.starts_with("the reviewer and the agent could not agree after 2 round(s)"), "{err}\n{log:#?}");
        assert!(err.contains("needs a person: The empty-list behaviour"), "{err}");
        assert!(log.iter().any(|l| l.starts_with("the reviewer asked twice; asking")), "{log:?}");
        assert!(err.contains("kept on branch"), "{err}");
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
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: Some(&claim), rounds: 1, reviewer: None, ask: None },
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
        let _ = fix(&repo, &engine, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: Some(&claim), rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {});
        assert!(claim.0.lock().unwrap()[1].starts_with("finish ABC-7 err the agent changed nothing"));
    }

    #[test]
    fn a_leftover_branch_is_refused_rather_than_reused() {
        let (_tmp, repo) = setup("true");
        repo.create_branch("fix/abc-7-total-is-off-by-one", false).unwrap();
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![]))));
        let err = fix(&repo, &engine, &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn a_sandbox_runtime_that_is_missing_is_refused_before_anything_runs() {
        let Some(missing) = crate::sandbox::Kind::ALL.into_iter().find(|k| !k.installed()) else {
            eprintln!("skipped: every runtime is installed here");
            return;
        };
        let (_tmp, repo) = setup("true");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let spec = crate::sandbox::Spec { kind: Some(missing), image: String::new() };
        let err = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: Some(&spec), claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("not installed"), "{err}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    /// The whole fix inside a real sandbox, on whatever runtime is here:
    /// the agent's command and the check both run in it, and the sandbox
    /// is gone afterwards.
    /// `cargo test --lib backlog::tests::live_sandbox -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_sandbox_a_fix_runs_its_command_and_its_check_inside() {
        if crate::sandbox::installed().is_empty() {
            eprintln!("no sandbox runtime; skipping");
            return;
        }
        // The check proves it ran in Linux, wherever the host is.
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py && uname -s | grep -q Linux && test -f linux-was-here");
        let engine = Engine::Harness(Box::new(Scripted(RefCell::new(vec![
            Reply { text: String::new(), calls: vec![ToolCall { id: "1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "lib.py", "old_text": "sum(xs) + 1", "new_text": "sum(xs)"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "2".into(), name: "run_command".into(), input: serde_json::json!({"command": "uname -a && touch linux-was-here"}) }], ..Default::default() },
            Reply { text: String::new(), calls: vec![ToolCall { id: "3".into(), name: "run_check".into(), input: serde_json::json!({"name": "tests"}) }], ..Default::default() },
            Reply { text: "fixed, inside the sandbox".into(), ..Default::default() },
        ]))));
        let spec = crate::sandbox::Spec::default();
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task(), base: "main", auth: None, instructions: None, sandbox: Some(&spec), claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| {
                println!("  {line}");
                log.push(line);
            },
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert!(log.iter().any(|l| l.starts_with("sandbox: ")), "{log:?}");
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        // The file the agent's command made inside is an artifact of the
        // run, not part of the change.
        assert_eq!(fixed.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["lib.py", "linux-was-here"]);
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
        let (title, body) = pull_request_text("- fixed it", &task(), &[CheckOutcome { name: "tests".into(), ok: true }], &["lint".to_string()], "Claude Code", 2, Some("Claude (opus)"));
        assert_eq!(title, "ABC-7: total() is off by one");
        assert!(body.contains("Resolves [ABC-7](https://acme.atlassian.net/browse/ABC-7)"));
        assert!(body.contains("> It adds 1."));
        assert!(body.contains("- fixed it"));
        assert!(body.contains("✅ `tests`"));
        assert!(body.contains("⏭ `lint` — fails on the base branch"), "{body}");
        assert!(body.contains("engine: Claude Code"), "{body}");
        assert!(body.contains("approved by Claude (opus) after 2 round(s)"), "{body}");
        let (_, none) = pull_request_text("x", &task(), &[], &[], "scripted", 1, None);
        assert!(none.contains("declares no checks"));

        // A prompt has no ticket to resolve: the prompt itself is quoted.
        let prompt = Task::from_prompt("add a --json flag to devdock status\n\nand cover it with a test");
        let (title, body) = pull_request_text("- added it", &prompt, &[], &[], "scripted", 1, None);
        assert_eq!(title, "add a --json flag to devdock status");
        assert!(body.starts_with("Asked in DevDock's Agent tab:\n\n> add a --json flag"), "{body}");
        assert!(body.contains("> and cover it with a test"));
        assert!(!body.contains("Resolves"));
        assert!(body.contains("in a worktree of its own"), "{body}");
    }

    #[test]
    fn the_task_carries_the_triage_plan() {
        let t = Triage { key: "ABC-7".into(), in_scope: true, area: "src/cli".into(), autonomous: true, confidence: 80, reason: "r".into(), plan: "edit cli.rs".into() };
        let text = task_text(&Task::from_issue(&issue(), Some(&t)), false);
        assert!(text.starts_with("Resolve this Jira ticket."));
        assert!(text.contains("ABC-7: total() is off by one"));
        assert!(text.contains("The work is in src/cli/."));
        assert!(text.contains("edit cli.rs"));
        assert!(text.contains("Nobody can answer questions"));
        let text = task_text(&Task::from_prompt("rename foo to bar"), false);
        let asking = task_text(&Task::from_prompt("rename foo to bar"), true);
        assert!(asking.contains("ask_developer") && !asking.contains("Nobody can answer"));
        assert!(text.starts_with("Do this task."), "{text}");
        assert!(text.contains("rename foo to bar"));
    }

    #[test]
    fn a_prompt_is_a_task_with_its_own_branch() {
        let t = Task::from_prompt("  Add a --json flag to `devdock status`!\n\nCover it with a test.  ");
        assert_eq!(t.label, "agent");
        assert_eq!(t.title, "Add a --json flag to `devdock status`!");
        assert_eq!(t.branch, "agent/add-a-json-flag-to-devdock-status");
        assert_eq!(t.brief, "Add a --json flag to `devdock status`!\n\nCover it with a test.");
        assert!(!t.is_ticket());
        assert_eq!(t.subject(), "Add a --json flag to `devdock status`!");
        assert_eq!(Task::from_prompt("!!!").branch, "agent/task");
        assert_eq!(Task::from_prompt("x").with_branch("fix/Historical Map").branch, "fix/Historical-Map");
        assert_eq!(Task::from_prompt("x").with_branch("  feature: new thing!  ").branch, "feature-new-thing");
        assert_eq!(Task::from_prompt("x").with_branch("   ").branch, "agent/x", "nothing typed keeps the default");
        let long = Task::from_prompt(&"word ".repeat(50));
        assert!(long.title.chars().count() <= 72);
        assert!(long.branch.len() <= "agent/".len() + 40);
        let ticket = task();
        assert!(ticket.is_ticket());
        assert_eq!(ticket.subject(), "ABC-7: total() is off by one");
        assert_eq!(ticket.branch, "fix/abc-7-total-is-off-by-one");
    }

    #[test]
    fn a_prompt_task_ends_as_a_branch_and_a_pull_request_like_a_ticket() {
        let (_tmp, repo) = setup("grep -q 'return sum(xs)$' lib.py");
        let engine = Engine::Harness(Box::new(fixing_provider()));
        let task = Task::from_prompt("total() is off by one: drop the stray + 1");
        let mut log = Vec::new();
        let fixed = fix(
            &repo,
            &engine,
            &Job { task: &task, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None },
            &fake_pr,
            &mut |line| log.push(line),
        )
        .unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        assert_eq!(fixed.branch, "agent/total-is-off-by-one-drop-the-stray-1");
        assert_eq!(fixed.key, "agent");
        assert_eq!(fixed.pr.title, "total() is off by one: drop the stray + 1");
        let subject = repo.log(1, Some(&fixed.branch)).unwrap()[0].subject.clone();
        assert_eq!(subject, "total() is off by one: drop the stray + 1");
        assert_eq!(repo.worktrees().unwrap().len(), 1, "the worktree is gone");
        assert!(!repo.worktree_default_path(&fixed.branch).exists());
        let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
        assert!(remote.contains("refs/heads/agent/total-is-off-by-one"), "{remote}");

        // A branch name the user typed that git would refuse is refused
        // here, before a worktree is made.
        let mut bad = Task::from_prompt("x");
        bad.branch = "agent/..oops".into();
        let err = fix(&repo, &engine, &Job { task: &bad, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 1, reviewer: None, ask: None }, &fake_pr, &mut |_| {}).unwrap_err();
        assert!(err.contains("not a valid branch name"), "{err}");
        assert_eq!(repo.worktrees().unwrap().len(), 1);
    }

    /// The whole pipeline with real models: a ticket that undersells the
    /// work, a check that knows the rest, a reviewer on a second model,
    /// and the advisor between rounds. Needs stored Claude credentials;
    /// `LIVE_ENGINE=claude-code` runs the fixer on Claude Code instead.
    /// `cargo test --lib backlog::tests::live_ -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_a_ticket_becomes_a_pull_request_through_rounds() {
        let Some(fixer_client) = crate::claude::Client::from_store("claude-haiku-4-5-20251001") else {
            eprintln!("no Claude credentials; skipping");
            return;
        };
        let Some(senior) = crate::claude::Client::from_store("claude-sonnet-5") else { return };
        let (_tmp, repo) = setup("python3 test_total.py");
        std::fs::write(
            repo.path().join("test_total.py"),
            "from lib import total\n\nassert total([1, 2]) == 3, total([1, 2])\ntry:\n    total([])\nexcept ValueError:\n    pass\nelse:\n    raise SystemExit('total([]) must raise ValueError')\nprint('ok')\n",
        )
        .unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-q", "-m", "add the test"]).unwrap();
        repo.git(&["push", "-q", "origin", "main"]).unwrap();

        let fixer = match std::env::var("LIVE_ENGINE").as_deref() {
            Ok("claude-code") => Engine::ClaudeCode(crate::agent::claude_code::Config::default()),
            _ => Engine::Harness(Box::new(fixer_client)),
        };
        let reviewer = Engine::Harness(Box::new(senior));
        // `LIVE_TICKET=undecided` words the ticket so the fixer is tempted
        // to ask a person, which is the advisor's cue.
        let undecided = std::env::var("LIVE_TICKET").as_deref() == Ok("undecided");
        let issue = BacklogIssue {
            key: "ABC-9".into(),
            summary: "total() gives the wrong answer".into(),
            description: if undecided {
                "total([1, 2]) returns 4. Also nobody has decided what total([]) should do — check with the product owner before implementing anything for the empty case.".into()
            } else {
                "total([1, 2]) returns 4. Make total() right; the test file in the repository says what right is.".into()
            },
            url: "https://acme.atlassian.net/browse/ABC-9".into(),
            ..Default::default()
        };
        let task = Task::from_issue(&issue, None);
        let mut log = Vec::new();
        let result = fix(
            &repo,
            &fixer,
            &Job { task: &task, base: "main", auth: None, instructions: None, sandbox: None, claim: None, rounds: 4, reviewer: Some(&reviewer), ask: None },
            &fake_pr,
            &mut |line| {
                println!("  {line}");
                log.push(line);
            },
        );
        let fixed = result.unwrap_or_else(|e| panic!("{e}\n{log:#?}"));
        println!("--- PR body ---\n{}", fixed.summary);
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        assert!(fixed.reviewed_by.is_some(), "the reviewer approved");
        assert_eq!(repo.worktrees().unwrap().len(), 1, "the worktree is gone");
        let lib = repo.git(&["show", &format!("{}:lib.py", fixed.branch)]).unwrap();
        assert!(lib.contains("ValueError"), "the hidden rule made it in:\n{lib}");
        assert!(!lib.contains("+ 1"), "{lib}");
        println!("rounds: {}, turns: {}, engine: {}, reviewed by: {:?}", fixed.rounds, fixed.turns, fixed.engine, fixed.reviewed_by);
    }
}
