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
        }
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
            body.push_str(&format!("- ⏭ `{name}` — fails on the base branch and fails the same way after this change; not counted\n"));
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
    on_event(format!("branch {branch} from {}", job.base));

    // 1. Worktree.
    {
        let _guard = WORKTREE_LOCK.lock().map_err(|_| "worktree lock poisoned".to_string())?;
        let exists = repo.branches().map(|b| b.local.iter().any(|br| br.name == branch)).unwrap_or(false);
        if exists {
            return Err(format!(
                "branch {branch} already exists; a previous attempt left it. Delete it \
                 (git branch -D {branch}), finish it by hand, or open its pull request."
            ));
        }
        repo.worktree_add(&dir, &branch, Some(job.base)).map_err(|e| e.to_string())?;
    }
    on_event(format!("worktree {}", dir.display()));
    if let Some(claim) = job.claim {
        for line in claim.start(&job.task.label) {
            on_event(line);
        }
    }

    let mut result = work(engine, job, &branch, &dir, publish, on_event);

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
        let result = crate::local_ci::run_job_with(&runners, wt.path(), &step);
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
    if !jobs.is_empty() {
        on_event("running the checks on the untouched tree first".into());
        for j in &jobs {
            let result = crate::local_ci::run_job_with(&runners, wt.path(), j);
            if result.ok {
                continue;
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
                "`{}` already fails on {} before any change: {}",
                j.name, job.base, first.chars().take(140).collect::<String>()
            ));
            baseline.insert(j.name.clone(), normalize_output(&result.output));
        }
    }
    let mut skipped: Vec<String> = Vec::new();

    let tracked = wt.tracked_files().map_err(|e| e.to_string())?;
    let mut workspace = Workspace::new(wt.path(), tracked.clone(), Access::ReadWrite)?
        .with_write_mode(WriteMode::Live)
        .with_checks(jobs.clone())
        .with_commands(true);
    if let Some(sandbox) = &sandbox {
        workspace = workspace.with_sandbox(sandbox.describe(), runners.clone());
    }
    if let Some(ask) = &job.ask {
        workspace = workspace.with_asker(ask.clone());
    }
    let base_task = task_text(job.task, job.ask.is_some());
    let mut context = String::from("This is an unattended run on a fresh worktree of the repository.");
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
    let mut history: Vec<coding::Turn> = Vec::new();
    let mut last_diff: Option<String> = None;
    let mut turns = 0;
    let mut summary;
    let mut checks: Vec<CheckOutcome> = Vec::new();
    let mut reviewed_by: Option<String> = None;
    let mut revises = 0;
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
                context: Some(&context),
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
        skipped.clear();
        let mut failed: Option<String> = None;
        for j in &jobs {
            on_event(format!("verifying: {}", j.name));
            let result = crate::local_ci::run_job_with(&runners, wt.path(), j);
            if !result.ok {
                if let Some(before) = baseline.get(&j.name) {
                    if *before == normalize_output(&result.output) {
                        on_event(format!("{} fails exactly as it did before the change; not counted", j.name));
                        skipped.push(j.name.clone());
                        continue;
                    }
                }
            }
            on_event(format!("{} {}", j.name, if result.ok { "passed" } else { "FAILED" }));
            checks.push(CheckOutcome { name: j.name.clone(), ok: result.ok });
            if !result.ok {
                // A program the check needs is not there: the environment's
                // fault, not the change's. Saying so beats another round.
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
    // where the checks ran, kept outside the repository, never part of
    // the change.
    let screenshots = capture_screenshots(&wt, &runners, sandbox.is_some(), branch, on_event);

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
        screenshots,
    })
}

/// Renders the first frame of every Flutter app in the tree through a
/// generated golden test — real fonts from the SDK's cache, a 1280×800
/// surface — and copies the image out to DevDock's own directory. The
/// generated test and its image are removed again. Anything that goes
/// wrong is logged and the run carries on: a screenshot is a courtesy.
fn capture_screenshots(
    wt: &Repo,
    runners: &crate::local_ci::runner::RunnerRegistry,
    sandboxed: bool,
    branch: &str,
    on_event: &mut dyn FnMut(String),
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (rel, package) in flutter_apps(wt.path()) {
        let dir = if rel.is_empty() { wt.path().to_path_buf() } else { wt.path().join(&rel) };
        let test_dir = dir.join("test");
        let test_file = test_dir.join("devdock_smoke_test.dart");
        let image = test_dir.join("devdock_smoke.png");
        if std::fs::create_dir_all(&test_dir).is_err() || std::fs::write(&test_file, smoke_test_source(&package)).is_err() {
            continue;
        }
        let job = crate::local_ci::Job {
            name: if rel.is_empty() { "screenshot".into() } else { format!("screenshot ({rel})") },
            commands: vec!["flutter test --update-goldens test/devdock_smoke_test.dart".into()],
            dir: rel.clone(),
            runner: sandboxed.then(|| crate::sandbox::RUNNER_ID.to_string()),
            timeout_secs: Some(600),
            ..Default::default()
        };
        on_event(format!("taking a screenshot of {}", if rel.is_empty() { "the app".to_string() } else { rel.clone() }));
        let result = crate::local_ci::run_job_with(runners, wt.path(), &job);
        let _ = std::fs::remove_file(&test_file);
        if !result.ok || !image.exists() {
            let why = result
                .output
                .lines()
                .rev()
                .find(|l| l.contains("Error") || l.contains("error") || l.contains("Exception"))
                .or_else(|| result.output.lines().rev().find(|l| !l.trim().is_empty()))
                .unwrap_or("no output")
                .trim();
            on_event(format!("no screenshot: the app did not render in a test ({})", why.chars().take(160).collect::<String>()));
            let _ = std::fs::remove_file(&image);
            continue;
        }
        let keep = crate::secure_store::config_dir().join("screenshots");
        let _ = std::fs::create_dir_all(&keep);
        let name = format!("{}{}.png", slugify(branch, 60), if rel.is_empty() { String::new() } else { format!("-{}", slugify(&rel, 30)) });
        let dest = keep.join(name);
        match std::fs::copy(&image, &dest) {
            Ok(_) => {
                on_event(format!("screenshot: {}", dest.display()));
                out.push(dest);
            }
            Err(e) => on_event(format!("could not keep the screenshot: {e}")),
        }
        let _ = std::fs::remove_file(&image);
    }
    out
}

/// Flutter apps in the tree — a `pubspec.yaml` with the Flutter SDK and a
/// `lib/main.dart` — as (directory relative to the root, package name).
fn flutter_apps(root: &Path) -> Vec<(String, String)> {
    let mut apps = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let pubspec = dir.join("pubspec.yaml");
        if let Ok(text) = std::fs::read_to_string(&pubspec) {
            if (text.contains("sdk: flutter") || text.contains("flutter:")) && dir.join("lib/main.dart").exists() {
                let name = text.lines().find_map(|l| l.strip_prefix("name:")).map(|n| n.trim().trim_matches('"').trim_matches('\'').to_string());
                if let Some(name) = name.filter(|n| !n.is_empty()) {
                    let rel = dir.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                    apps.push((rel, name));
                }
            }
            continue;
        }
        if depth >= 3 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() && !name.starts_with('.') && !matches!(name.as_str(), "build" | "node_modules" | "ios" | "android" | "macos" | "linux" | "windows" | "web" | "test") {
                stack.push((path, depth + 1));
            }
        }
    }
    apps.sort();
    apps
}

/// The test that renders the app's first frame to `devdock_smoke.png`.
fn smoke_test_source(package: &str) -> String {
    format!(
        r#"// Generated by DevDock for one screenshot; removed afterwards.
import 'dart:io';
import 'dart:typed_data';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:{package}/main.dart' as app;

Future<void> _loadFonts() async {{
  final root = Platform.environment['FLUTTER_ROOT'];
  if (root == null) return;
  final dir = '$root/bin/cache/artifacts/material_fonts';
  Future<void> load(String family, String file) async {{
    final f = File('$dir/$file');
    if (!await f.exists()) return;
    final bytes = await f.readAsBytes();
    final loader = FontLoader(family)..addFont(Future.value(ByteData.view(bytes.buffer)));
    await loader.load();
  }}
  await load('Roboto', 'Roboto-Regular.ttf');
  await load('Roboto', 'Roboto-Medium.ttf');
  await load('Roboto', 'Roboto-Bold.ttf');
  await load('MaterialIcons', 'MaterialIcons-Regular.otf');
}}

void main() {{
  testWidgets('devdock smoke screenshot', (tester) async {{
    WidgetsApp.debugAllowBannerOverride = false;
    tester.view.physicalSize = const Size(1280, 800);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);
    // Real I/O — font files, whatever main awaits — only completes outside
    // the test's fake-async zone.
    await tester.runAsync(() async {{
      try {{
        await _loadFonts();
      }} catch (_) {{}}
      try {{
        // Through dynamic: main may return void or a Future, either is fine.
        final dynamic started = (app.main as dynamic)();
        if (started is Future) {{
          await started.timeout(const Duration(seconds: 15));
        }}
      }} catch (_) {{}}
    }});
    await tester.pump();
    try {{
      await tester.pumpAndSettle(const Duration(milliseconds: 100), EnginePhase.sendSemanticsUpdate, const Duration(seconds: 10));
    }} catch (_) {{}}
    final root = find.byType(WidgetsApp);
    expect(root, findsWidgets, reason: 'the app put no WidgetsApp on screen');
    await expectLater(root.first, matchesGoldenFile('devdock_smoke.png'));
  }});
}}
"#
    )
}

/// Reviews the change with whichever engine: the harness over a read-only
/// workspace on the changed tree, or Claude Code with reading tools only.
fn review_with(
    reviewer: &Engine,
    root: &Path,
    tracked: &[String],
    brief: &str,
    diff: &str,
    on_event: &mut dyn FnMut(String),
) -> Result<crate::agent::backlog::Verdict, String> {
    match reviewer {
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
    }
}

/// Asks a second agent what to do about a round that did not get through:
/// the harness over a read-only workspace, or Claude Code reading only.
fn advise_with(
    advisor: &Engine,
    root: &Path,
    tracked: &[String],
    brief: &str,
    happened: &str,
    diff: &str,
    on_event: &mut dyn FnMut(String),
) -> Result<crate::agent::backlog::Advice, String> {
    match advisor {
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
    }
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
        "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", "target", "node_modules",
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
    let subject = format!("WIP: {} (not accepted)", truncate(&task.subject(), 50));
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
        assert_eq!(log.first().map(String::as_str), Some("branch fix/abc-7-total-is-off-by-one from main"));
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
        assert!(log.iter().any(|l| l.starts_with("`lint` already fails on main before any change: lib.py:1: style")), "{log:?}");
        assert!(log.iter().any(|l| l == "lint fails exactly as it did before the change; not counted"), "{log:?}");
        assert_eq!(fixed.skipped, ["lint"]);
        assert_eq!(fixed.checks, [CheckOutcome { name: "tests".into(), ok: true }]);
        assert!(fixed.pr.number == 42);
    }

    #[test]
    fn flutter_apps_are_found_and_the_smoke_test_names_their_package() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("mobile/lib")).unwrap();
        fs::write(root.join("mobile/pubspec.yaml"), "name: farm_app\ndependencies:\n  flutter:\n    sdk: flutter\n").unwrap();
        fs::write(root.join("mobile/lib/main.dart"), "void main() {}\n").unwrap();
        fs::create_dir_all(root.join("rules/lib")).unwrap();
        fs::write(root.join("rules/pubspec.yaml"), "name: rules\nenvironment:\n  sdk: ^3.0.0\n").unwrap();
        assert_eq!(flutter_apps(root), [("mobile".to_string(), "farm_app".to_string())], "a pure Dart package has no screen");
        let source = smoke_test_source("farm_app");
        assert!(source.contains("import 'package:farm_app/main.dart' as app;"));
        assert!(source.contains("matchesGoldenFile('devdock_smoke.png')"));
        assert!(source.contains("Roboto-Regular.ttf"), "real fonts, not boxes");
    }

    /// A real Flutter app's first frame, rendered in the sandbox by the
    /// generated golden test and kept as a PNG DevDock can show.
    /// `cargo test --lib backlog::tests::live_screenshot -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_screenshot_of_a_flutter_app_is_taken_in_the_sandbox() {
        if crate::sandbox::installed().is_empty() {
            eprintln!("no sandbox runtime; skipping");
            return;
        }
        let (_tmp, repo) = setup("true");
        let root = repo.path().to_path_buf();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("pubspec.yaml"), "name: hello_app\nenvironment:\n  sdk: ^3.0.0\ndependencies:\n  flutter:\n    sdk: flutter\ndev_dependencies:\n  flutter_test:\n    sdk: flutter\nflutter:\n  uses-material-design: true\n").unwrap();
        fs::write(root.join("lib/main.dart"), "import 'package:flutter/material.dart';\n\nvoid main() => runApp(const HelloApp());\n\nclass HelloApp extends StatelessWidget {\n  const HelloApp({super.key});\n  @override\n  Widget build(BuildContext context) => MaterialApp(\n    home: Scaffold(\n      appBar: AppBar(title: const Text('Hello from DevDock')),\n      body: const Center(child: Text('The first frame, rendered in the sandbox.', style: TextStyle(fontSize: 24))),\n      floatingActionButton: FloatingActionButton(onPressed: () {}, child: const Icon(Icons.camera_alt)),\n    ),\n  );\n}\n").unwrap();
        sh(&root, &["add", "-A"]);
        sh(&root, &["commit", "-q", "-m", "a flutter app"]);
        let mut log = |l: String| println!("  {l}");
        let sandbox = crate::sandbox::Sandbox::start(&crate::sandbox::Spec::default(), &root, &mut log).unwrap();
        sandbox.provision(&["flutter"], &mut log).unwrap();
        let sandbox = std::sync::Arc::new(sandbox);
        let mut runners = crate::local_ci::runner::RunnerRegistry::with_builtins();
        runners.register(Box::new(crate::sandbox::SandboxRunner(sandbox.clone())));
        for mut step in crate::local_ci::prepare_jobs(&root, true) {
            step.runner = Some(crate::sandbox::RUNNER_ID.into());
            let r = crate::local_ci::run_job_with(&runners, &root, &step);
            assert!(r.ok, "{}: {}", step.name, r.output);
        }
        let shots = capture_screenshots(&repo, &runners, true, "agent/live-screenshot", &mut log);
        assert_eq!(shots.len(), 1, "one app, one screenshot");
        let bytes = fs::read(&shots[0]).unwrap();
        let img = image::load_from_memory(&bytes).expect("a PNG");
        println!("screenshot {} is {}x{}", shots[0].display(), img.width(), img.height());
        assert!(img.width() >= 800 && img.height() >= 500, "{}x{}", img.width(), img.height());
        assert!(!root.join("test/devdock_smoke_test.dart").exists() && !root.join("test/devdock_smoke.png").exists(), "the generated files are gone");
        let status = repo.git(&["status", "--porcelain"]).unwrap();
        assert!(!status.contains("devdock_smoke"), "the generated files are gone: {status}");
        assert!(!status.lines().any(|l| l.starts_with(" M") || l.starts_with("M ")), "nothing tracked changed: {status}");
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
