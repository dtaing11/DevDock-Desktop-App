//! The Jira backlog: tickets nobody has picked up, judged for what an agent
//! could do, and the agents doing it.
//!
//! The dialog is in three parts. The tickets, with what triage concluded
//! about each — this repository or not, which part of it, doable unattended
//! or needs a person, and why. A selection, of any of them. And the agents:
//! one per selected ticket, each in its own worktree, each with its state
//! (queued, running, done, failed), everything it did as it did it, and, at
//! the end, the files it changed and the draft pull request it opened.
//!
//! Nothing here touches Jira, and nothing reaches the main checkout: a fix
//! is a branch and a draft pull request, and the worktree it was made in is
//! removed when it is done. The developer accepts the pull request or not.

use super::worker::{self, strerr, Msg};
use super::{theme, App, Dialog};
use crate::agent::backlog::Triage;
use crate::jira::BacklogIssue;
use egui::{RichText, ScrollArea};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

/// Where one ticket's agent is.
#[derive(Debug)]
pub enum RunState {
    Queued,
    Running,
    Done(Box<crate::backlog::Fixed>),
    Failed(String),
}

/// One agent working one ticket, as the dialog tracks it.
#[derive(Debug)]
pub struct TicketRun {
    /// What it is on: the ticket's summary, or the prompt's first line.
    pub title: String,
    pub state: RunState,
    /// The branch a failed run kept its attempt on, unpushed, for a person
    /// to finish.
    pub kept: Option<String>,
    /// Everything it did, one line per tool call, check, commit, and push.
    pub log: Vec<String>,
    pub started: Option<Instant>,
    pub took: Option<Duration>,
}

/// The branch a failed run's reason says the attempt was kept on. The
/// fixer writes that line; this reads it back.
pub fn kept_branch(reason: &str) -> Option<String> {
    const MARK: &str = "the attempt is kept on branch ";
    let at = reason.find(MARK)?;
    reason[at + MARK.len()..].split_whitespace().next().map(str::to_string)
}

impl TicketRun {
    pub fn queued(title: impl Into<String>) -> Self {
        Self { title: title.into(), state: RunState::Queued, kept: None, log: Vec::new(), started: None, took: None }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running)
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, RunState::Done(_) | RunState::Failed(_))
    }

    /// Time spent so far, or in total.
    pub fn elapsed(&self) -> Option<Duration> {
        self.took.or_else(|| self.started.map(|s| s.elapsed()))
    }
}

/// Everything the backlog dialog owns.
pub struct BacklogState {
    /// Jira project key.
    pub project: String,
    /// How many tickets to fetch.
    pub max: usize,
    pub loading: bool,
    pub triaging: bool,
    pub error: Option<String>,
    pub issues: Vec<BacklogIssue>,
    pub triage: BTreeMap<String, Triage>,
    pub triage_log: Vec<String>,
    /// Ticket keys ticked for fixing.
    pub selected: BTreeSet<String>,
    /// One entry per ticket an agent has been started on.
    pub runs: BTreeMap<String, TicketRun>,
    /// Tickets waiting for a free slot.
    pub queue: VecDeque<String>,
    /// How many agents run at once.
    pub parallel: usize,
    /// Show only what triage suggested.
    pub only_suggested: bool,
    /// Run every check inside this Docker image, when set.
    pub sandbox: bool,
    pub sandbox_image: String,
    /// Assign a ticket to me, move it to the active sprint, and mark it In
    /// Progress when its agent starts; comment when it finishes.
    pub claim: bool,
    /// How many attempts a ticket gets: a failed check or a reviewer's
    /// "revise" goes back to the agent with the reason.
    pub rounds: usize,
    /// A second agent reads the ticket and the diff before the pull
    /// request, with the code-review model.
    pub review: bool,
    /// Whose log is unfolded.
    pub expanded: Option<String>,
}

impl Default for BacklogState {
    fn default() -> Self {
        Self {
            project: String::new(),
            max: 40,
            loading: false,
            triaging: false,
            error: None,
            issues: Vec::new(),
            triage: BTreeMap::new(),
            triage_log: Vec::new(),
            selected: BTreeSet::new(),
            runs: BTreeMap::new(),
            queue: VecDeque::new(),
            parallel: 3,
            only_suggested: false,
            sandbox: false,
            sandbox_image: String::new(),
            claim: true,
            rounds: 3,
            review: true,
            expanded: None,
        }
    }
}

impl BacklogState {
    pub fn running(&self) -> usize {
        self.runs.values().filter(|r| r.is_running()).count()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn done(&self) -> usize {
        self.runs.values().filter(|r| matches!(r.state, RunState::Done(_))).count()
    }

    pub fn failed(&self) -> usize {
        self.runs.values().filter(|r| matches!(r.state, RunState::Failed(_))).count()
    }

    /// Tickets triage thinks an agent could take.
    pub fn suggested(&self) -> Vec<&BacklogIssue> {
        self.issues
            .iter()
            .filter(|i| self.triage.get(&i.key).is_some_and(Triage::suggested))
            .collect()
    }

    /// Whether a ticket can be ticked: not already taken by an agent.
    fn selectable(&self, key: &str) -> bool {
        !self.runs.contains_key(key) && !self.queue.iter().any(|k| k == key)
    }
}

impl App {
    /// Opens the backlog, connecting to Jira first when needed.
    pub fn open_backlog(&mut self) {
        if self.repo.is_none() {
            self.dialog = Dialog::RepoPicker;
            return;
        }
        self.dialog = Dialog::Backlog;
        if self.tickets.creds.site.is_empty() {
            if let Some(creds) = crate::jira::CredentialStore::load() {
                self.tickets.creds = creds;
            }
        }
        if self.tickets.account.is_none() && self.tickets.creds.is_complete() {
            self.connect_jira();
        }
        if self.backlog.project.is_empty() {
            self.backlog.project = self.tickets.project.clone();
        }
        // Default to a sandbox that matches the repository, when Docker is
        // there to run it. The user can turn it off or name another image.
        if self.backlog.sandbox_image.is_empty() {
            if let Some(repo) = self.repo.as_ref() {
                let tracked = repo.tracked_files().unwrap_or_default();
                if let Some(image) = crate::backlog::suggest_sandbox_image(&tracked) {
                    self.backlog.sandbox_image = image.to_string();
                    self.backlog.sandbox = crate::local_ci::docker_available();
                }
            }
        }
        if self.tickets.account.is_some() && self.backlog.issues.is_empty() && !self.backlog.loading {
            self.load_backlog();
        }
    }

    /// Fetches the project's unassigned tickets, then judges them.
    pub fn load_backlog(&mut self) {
        let project = self.backlog.project.trim().to_string();
        if project.is_empty() {
            self.backlog.error = Some("Pick a project first.".into());
            return;
        }
        let max = self.backlog.max;
        self.backlog.loading = true;
        self.backlog.error = None;
        self.backlog.triage.clear();
        self.backlog.triage_log.clear();
        self.worker.spawn(move || {
            let result = crate::jira::Client::from_store()
                .ok_or_else(|| "Not connected to Jira.".to_string())
                .and_then(|c| c.unassigned_backlog(&project, max).map_err(|e| e.to_string()));
            Msg::BacklogIssues(result)
        });
    }

    /// Asks the model which tickets are this repository's and doable.
    pub fn triage_backlog(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        if self.backlog.issues.is_empty() || self.backlog.triaging {
            return;
        }
        // Judging is a read-only harness run, so it needs a model, not
        // Claude Code; when the fixer is Claude Code, the nearest task's
        // model judges.
        let Some(sel) = self.triage_selection() else {
            self.backlog.error = Some(
                "Judging the backlog needs a Claude or Ollama model: pick one for Jira \
                 tickets or the coding agent."
                    .into(),
            );
            return;
        };
        let issues = self.backlog.issues.clone();
        let url = self.effective_ollama_url();
        let progress = self.worker.progress();
        self.backlog.triaging = true;
        self.backlog.error = None;
        self.backlog.triage_log.clear();
        self.worker.spawn(move || {
            let result = (|| -> Result<Vec<Triage>, String> {
                let provider = super::agent_provider(&sel, &url)?;
                let tracked = strerr(repo.tracked_files())?;
                let mut workspace = crate::agent::Workspace::new(
                    repo.path(),
                    tracked,
                    crate::agent::Access::ReadOnly,
                )?;
                crate::agent::backlog::run(provider.as_ref(), &mut workspace, &issues, &mut |event| {
                    progress.send(Msg::BacklogTriageEvent(event.line()));
                })
            })();
            Msg::BacklogTriage(result)
        });
    }

    /// A model for judging: the fixer's, unless that is Claude Code, then
    /// the first other task with a model.
    fn triage_selection(&self) -> Option<super::AiSelection> {
        [
            worker::AiTarget::Backlog,
            worker::AiTarget::Tickets,
            worker::AiTarget::Coding,
            worker::AiTarget::Review,
            worker::AiTarget::Commit,
        ]
        .into_iter()
        .filter_map(|t| self.ai_selection(t))
        .find(|s| s.provider != crate::agent::claude_code::PROVIDER)
    }

    /// Ticks every ticket triage suggested.
    pub fn backlog_select_suggested(&mut self) {
        let keys: Vec<String> = self.backlog.suggested().iter().map(|i| i.key.clone()).collect();
        for key in keys {
            if self.backlog.selectable(&key) {
                self.backlog.selected.insert(key);
            }
        }
    }

    /// Starts an agent on every ticked ticket, `parallel` at a time.
    pub fn backlog_fix_selected(&mut self) {
        if self.ai_selection(worker::AiTarget::Backlog).is_none() {
            self.toast("Pick a model for the backlog fixer first.", true);
            return;
        }
        if crate::github::Client::from_store().is_none() {
            self.toast("Sign in to GitHub first: the fix ends as a pull request.", true);
            return;
        }
        let keys: Vec<String> = std::mem::take(&mut self.backlog.selected)
            .into_iter()
            .filter(|k| self.backlog.selectable(k))
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let title = self.backlog.issues.iter().find(|i| i.key == key).map(|i| i.summary.clone()).unwrap_or_default();
            self.backlog.runs.insert(key.clone(), TicketRun::queued(title));
            self.backlog.queue.push_back(key);
        }
        self.pump_backlog();
    }

    /// Fills free slots from the queue.
    fn pump_backlog(&mut self) {
        while self.backlog.running() < self.backlog.parallel.max(1) {
            let Some(key) = self.backlog.queue.pop_front() else { break };
            self.start_backlog_fix(key);
        }
    }

    fn start_backlog_fix(&mut self, key: String) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(issue) = self.backlog.issues.iter().find(|i| i.key == key).cloned() else {
            return;
        };
        let Some(sel) = self.ai_selection(worker::AiTarget::Backlog) else { return };
        let triage = self.backlog.triage.get(&key).cloned();
        let url = self.effective_ollama_url();
        let token = self.gh_token();
        let instructions = self.coding_instructions();
        let sandbox = self.backlog.sandbox.then(|| self.backlog.sandbox_image.trim().to_string()).filter(|s| !s.is_empty());
        let claim = self.backlog.claim;
        let rounds = self.backlog.rounds.max(1);
        // The reviewer is the code-review task's model — the one already
        // chosen for judging changes — falling back to the fixer's own
        // engine as a fresh session.
        let review_sel = self.backlog.review.then(|| {
            self.ai_selection(worker::AiTarget::Review).unwrap_or_else(|| sel.clone())
        });
        let project = self.backlog.project.clone();
        let progress = self.worker.progress();
        if let Some(run) = self.backlog.runs.get_mut(&key) {
            run.state = RunState::Running;
            run.started = Some(Instant::now());
        }
        let done_key = key.clone();
        self.worker.spawn(move || {
            let result = (|| -> Result<crate::backlog::Fixed, String> {
                let engine = super::agent_engine(&sel, &url)?;
                let reviewer = match &review_sel {
                    Some(r) => Some(super::agent_engine(r, &url)?),
                    None => None,
                };
                let client = crate::github::Client::from_store().ok_or("Not signed in to GitHub")?;
                let slug = super::views::origin_slug(&repo).ok_or("No github.com remote found")?;
                let base = crate::stack::default_branch(&repo);
                // A stale base makes a pull request that reverts other
                // people's work. Best effort: offline still works.
                let _ = repo.fetch(token.as_deref());
                let publish = |title: &str, body: &str, head: &str| {
                    client
                        .create_draft_pull_request(&slug, title, body, head, &base)
                        .map_err(|e| e.to_string())
                };
                // Claiming needs who I am and where the sprint is; both
                // best effort, and neither can stop the fix.
                let claimer = if claim {
                    crate::jira::Client::from_store().and_then(|client| {
                        let account_id = client.myself().ok()?.account_id;
                        let sprint = client.active_sprint(&project).ok().flatten();
                        Some(crate::backlog::JiraClaim { client, account_id, sprint })
                    })
                } else {
                    None
                };
                if claim && claimer.is_none() {
                    progress.send(Msg::BacklogProgress {
                        key: issue.key.clone(),
                        line: "could not look up your Jira account; the ticket is not claimed".into(),
                    });
                }
                let task = crate::backlog::Task::from_issue(&issue, triage.as_ref());
                let job = crate::backlog::Job {
                    task: &task,
                    base: &base,
                    auth: token.as_deref(),
                    instructions: instructions.as_deref(),
                    sandbox_image: sandbox.as_deref(),
                    claim: claimer.as_ref().map(|c| c as &dyn crate::backlog::Claimer),
                    rounds,
                    reviewer: reviewer.as_ref(),
                };
                let key = issue.key.clone();
                crate::backlog::fix(&repo, &engine, &job, &publish, &mut |line| {
                    progress.send(Msg::BacklogProgress { key: key.clone(), line });
                })
            })();
            Msg::BacklogDone { key: done_key, result: result.map(Box::new) }
        });
    }

    /// Forgets finished agents so the list is the live ones again.
    pub fn backlog_clear_finished(&mut self) {
        self.backlog.runs.retain(|_, r| !r.is_finished());
    }

    pub(super) fn on_backlog_issues(&mut self, result: Result<Vec<BacklogIssue>, String>) {
        self.backlog.loading = false;
        match result {
            Ok(issues) => {
                self.backlog.issues = issues;
                self.backlog.selected.clear();
                if self.backlog.issues.is_empty() {
                    self.backlog.error = None;
                } else {
                    self.triage_backlog();
                }
            }
            Err(e) => self.backlog.error = Some(e),
        }
    }

    pub(super) fn on_backlog_triage(&mut self, result: Result<Vec<Triage>, String>) {
        self.backlog.triaging = false;
        match result {
            Ok(triage) => {
                self.backlog.triage = triage.into_iter().map(|t| (t.key.clone(), t)).collect();
                let n = self.backlog.suggested().len();
                self.toast(format!("{n} ticket(s) the agent could take."), false);
            }
            Err(e) => self.backlog.error = Some(e),
        }
    }

    pub(super) fn on_backlog_progress(&mut self, key: String, line: String) {
        if let Some(run) = self.backlog.runs.get_mut(&key) {
            run.log.push(line);
        }
    }

    pub(super) fn on_backlog_done(&mut self, key: String, result: Result<Box<crate::backlog::Fixed>, String>) {
        if let Some(run) = self.backlog.runs.get_mut(&key) {
            run.took = run.started.map(|s| s.elapsed());
            run.state = match result {
                Ok(fixed) => {
                    run.log.push(format!("done: {}", fixed.pr.html_url));
                    RunState::Done(fixed)
                }
                Err(e) => {
                    run.log.push(format!("failed: {}", e.lines().next().unwrap_or("")));
                    run.kept = kept_branch(&e);
                    RunState::Failed(e)
                }
            };
        }
        self.pump_backlog();
        if self.backlog.running() == 0 && self.backlog.queue.is_empty() {
            let (done, failed) = (self.backlog.done(), self.backlog.failed());
            self.toast(
                format!("Backlog agents finished: {done} pull request(s) opened, {failed} failed."),
                failed > 0 && done == 0,
            );
        }
        // New branches exist now.
        self.refresh();
    }
}

/// A label that wraps to the width it has instead of widening the dialog.
pub(super) fn wrapped(ui: &mut egui::Ui, text: RichText) {
    ui.add(egui::Label::new(text).wrap());
}

/// The first `max` characters of a line, with an ellipsis when cut.
pub(super) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

pub(super) fn mmss(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The dialog.
pub fn dialog(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    super::dialogs::modal(ctx, "Jira backlog", open, |ui| {
        // A definite width, from the screen rather than from the content:
        // rows and labels wrap to it, so the layout is the same every frame.
        // Sizing to wrapped content instead oscillates — wider makes a row
        // fit on one line, which makes the content narrower, which wraps the
        // row again — and with a dropdown open that is visible as twitching.
        let width = (ctx.screen_rect().width() * 0.85 - 48.0).clamp(460.0, 920.0);
        ui.set_width(width);

        if app.tickets.account.is_none() {
            super::dialogs::jira_connect(app, ui);
            return;
        }
        // Connected: the project picker, the model, and how to run.
        header(app, ui);
        ui.add_space(theme::UNIT);
        ui.separator();
        ui.add_space(theme::UNIT);

        if let Some(error) = app.backlog.error.clone() {
            wrapped(ui, RichText::new(error).color(theme::danger()));
            ui.add_space(theme::UNIT);
        }
        if app.backlog.loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(theme::SPINNER));
                ui.label(RichText::new("Reading the backlog…").color(theme::fg_dim()));
            });
            return;
        }
        if app.backlog.triaging {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(theme::SPINNER));
                ui.label(
                    RichText::new(format!("Judging {} ticket(s)…", app.backlog.issues.len()))
                        .color(theme::fg_dim()),
                );
            });
            if let Some(last) = app.backlog.triage_log.last() {
                ui.label(RichText::new(last).monospace().size(theme::SMALL).color(theme::fg_dim()));
            }
            ui.ctx().request_repaint();
        }
        if app.backlog.issues.is_empty() && !app.backlog.loading {
            ui.label(
                RichText::new("No unassigned, open tickets in this project.").color(theme::fg_dim()),
            );
        }

        // The agents, when any have been started.
        if !app.backlog.runs.is_empty() {
            agents(app, ui);
            ui.add_space(theme::UNIT);
            ui.separator();
            ui.add_space(theme::UNIT);
        }

        // The tickets.
        tickets(app, ui);
    });
}

fn header(app: &mut App, ui: &mut egui::Ui) {
    let account = app.tickets.account.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(RichText::new(&account).font(theme::semibold(theme::TEXT)));
        ui.label(
            RichText::new(app.tickets.creds.site.replace("https://", ""))
                .size(theme::SMALL)
                .color(theme::fg_dim()),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Sign out").clicked() {
                app.disconnect_jira();
            }
        });
    });
    ui.add_space(theme::UNIT);
    ui.horizontal(|ui| {
        ui.label("Project");
        let selected = app
            .tickets
            .projects
            .iter()
            .find(|p| p.key == app.backlog.project)
            .map(|p| format!("{} — {}", p.key, p.name))
            .unwrap_or_else(|| {
                if app.backlog.project.is_empty() { "Pick one…".into() } else { app.backlog.project.clone() }
            });
        let projects = app.tickets.projects.clone();
        let mut chosen: Option<String> = None;
        egui::ComboBox::from_id_salt("backlog-project")
            .selected_text(RichText::new(selected).size(theme::TEXT))
            .width(260.0)
            .show_ui(ui, |ui| {
                for project in &projects {
                    let label = format!("{} — {}", project.key, project.name);
                    if ui.selectable_label(project.key == app.backlog.project, label).clicked() {
                        chosen = Some(project.key.clone());
                    }
                }
            });
        if let Some(key) = chosen {
            if key != app.backlog.project {
                app.backlog.project = key;
                app.backlog.issues.clear();
                app.load_backlog();
            }
        }
        let busy = app.backlog.loading || app.backlog.triaging;
        if ui.add_enabled(!busy, egui::Button::new("Reload")).on_hover_text("Fetch the backlog again and judge it").clicked() {
            app.load_backlog();
        }
        if !app.backlog.issues.is_empty()
            && ui
                .add_enabled(!busy, egui::Button::new("Judge again"))
                .on_hover_text("Ask the model again, with the same tickets")
                .clicked()
        {
            app.triage_backlog();
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.push_id("backlog-fixer-engine", |ui| {
            super::views::engine_toggle(app, ui, worker::AiTarget::Backlog);
        });
        ui.add_space(theme::UNIT);
        ui.label("Fixer model");
        // Two pickers in one dialog: each in its own id scope, or egui sees
        // one widget twice and the second acts on the first.
        ui.push_id("backlog-fixer-model", |ui| {
            super::views::ai_model_picker(app, ui, worker::AiTarget::Backlog);
        });
        ui.add_space(theme::UNIT);
        ui.label("At once");
        ui.add(egui::Slider::new(&mut app.backlog.parallel, 1..=6).show_value(true))
            .on_hover_text("How many agents run in parallel, each in its own worktree");
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Rounds");
        ui.add(egui::Slider::new(&mut app.backlog.rounds, 1..=10).show_value(true)).on_hover_text(
            "How many attempts a ticket gets. A failed check, or a reviewer asking for \
             changes, goes back to the agent with the reason and a second agent's advice \
             on what went wrong; an agent that gave up is sent back too, unless the second \
             agent agrees it needs a person. Stops early when a round changes nothing new. \
             No draft pull request is opened while a check fails.",
        );
        ui.add_space(theme::UNIT);
        ui.checkbox(&mut app.backlog.review, "Second agent reviews").on_hover_text(
            "Before the pull request, the code-review model reads the ticket and the diff \
             and answers approve or revise. Revise is another round. Pick its model under \
             Reviewer, or in Settings for code review.",
        );
        if app.backlog.review {
            ui.push_id("backlog-reviewer-engine", |ui| {
                super::views::engine_toggle(app, ui, worker::AiTarget::Review);
            });
            ui.label("Reviewer");
            ui.push_id("backlog-reviewer-model", |ui| {
                super::views::ai_model_picker(app, ui, worker::AiTarget::Review);
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut app.backlog.claim, "Claim tickets I start").on_hover_text(
            "When an agent starts on a ticket: assign it to you, move it into the \
             active sprint, and mark it In Progress. When it finishes: a comment with \
             the draft pull request, or with why it could not. Off means Jira is never \
             written to.",
        );
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut app.backlog.sandbox, "Run checks in a sandbox").on_hover_text(
            "Every build and test the agent triggers runs inside this Docker image with \
             the worktree mounted at /work, so it cannot touch the machine.",
        );
        if app.backlog.sandbox {
            ui.add(
                egui::TextEdit::singleline(&mut app.backlog.sandbox_image)
                    .hint_text(super::views::dim_hint("rust:1.80, python:3.12, node:22…"))
                    .desired_width(220.0),
            );
            if !crate::local_ci::docker_available() {
                ui.label(RichText::new("Docker not found").size(theme::SMALL).color(theme::danger()));
            }
        }
    });
}

/// The agents: one card each, with state, elapsed time, what it is doing
/// now, and everything it did on request.
fn agents(app: &mut App, ui: &mut egui::Ui) {
    let (running, queued, done, failed) =
        (app.backlog.running(), app.backlog.queued(), app.backlog.done(), app.backlog.failed());
    ui.horizontal(|ui| {
        ui.label(theme::overline("AGENTS"));
        ui.label(
            RichText::new(format!(
                "{running} running · {queued} queued · {done} done · {failed} failed"
            ))
            .size(theme::SMALL)
            .color(theme::fg_dim()),
        );
        if running > 0 {
            ui.add(egui::Spinner::new().size(theme::SPINNER));
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if (done + failed) > 0 && ui.small_button("Clear finished").clicked() {
                app.backlog_clear_finished();
            }
        });
    });
    ui.add_space(theme::UNIT);

    let keys: Vec<String> = app.backlog.runs.keys().cloned().collect();
    ScrollArea::vertical().max_height(260.0).id_salt("backlog-agents").show(ui, |ui| {
        for key in keys {
            agent_card(app, ui, &key);
            ui.add_space(theme::UNIT);
        }
    });
}

fn agent_card(app: &mut App, ui: &mut egui::Ui, key: &str) {
    let Some(run) = app.backlog.runs.get(key) else { return };
    let expanded = app.backlog.expanded.as_deref() == Some(key);
    match run_card(ui, key, &run.title, run, expanded, "backlog-log") {
        CardAction::None => {}
        CardAction::ToggleLog => app.backlog.expanded = if expanded { None } else { Some(key.to_string()) },
        CardAction::OpenAttempt(branch) => app.open_attempt_in_vscode(&branch),
    }
}

/// What a click on a card asks for.
pub(super) enum CardAction {
    None,
    ToggleLog,
    /// Check the kept branch out and open it in Visual Studio Code.
    OpenAttempt(String),
}

/// One agent's card: its state, what it is doing or did, its log on
/// request, and — when it is done — the pull request and the files. Shared
/// by the backlog dialog and the Agent tab's worktree runs.
pub(super) fn run_card(ui: &mut egui::Ui, key: &str, title: &str, run: &TicketRun, expanded: bool, salt: &str) -> CardAction {
    let (state_label, color) = match &run.state {
        RunState::Queued => ("queued", theme::fg_dim()),
        RunState::Running => ("running", theme::ember()),
        RunState::Done(_) => ("done", theme::add()),
        RunState::Failed(_) => ("failed", theme::danger()),
    };
    let elapsed = run.elapsed().map(mmss);
    let last = run.log.last().cloned().unwrap_or_default();
    let mut action = CardAction::None;

    let width = ui.available_width();
    egui::Frame::new()
        .fill(theme::panel2())
        .stroke(egui::Stroke::new(1.0_f32, theme::border()))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(width - 20.0);
            // The state and the buttons share one row; the title, which
            // can be long, gets its own and wraps, so the buttons never
            // draw over it.
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("[{state_label}]")).color(color).monospace().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(elapsed) = elapsed {
                        ui.label(RichText::new(elapsed).monospace().size(theme::SMALL).color(theme::fg_dim()));
                    }
                    let toggle = if expanded { "Hide log".to_string() } else { format!("Log ({})", run.log.len()) };
                    if ui.small_button(toggle).clicked() {
                        action = CardAction::ToggleLog;
                    }
                    if !run.log.is_empty()
                        && ui.small_button("Copy log").on_hover_text("The whole log, to paste somewhere").clicked()
                    {
                        ui.ctx().copy_text(run.log.join("\n"));
                    }
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(key).monospace().strong());
                if !title.is_empty() {
                    ui.label(RichText::new(title).color(theme::fg()));
                }
            });
            if !expanded && !last.is_empty() {
                wrapped(ui, RichText::new(clip(&last, 240)).monospace().size(theme::SMALL).color(theme::fg_dim()));
            }
            if expanded {
                ScrollArea::vertical().max_height(180.0).id_salt((salt, key)).stick_to_bottom(true).show(ui, |ui| {
                    for line in &run.log {
                        let color = if line.starts_with('!') || line.starts_with("failed") {
                            theme::danger()
                        } else {
                            theme::fg_dim()
                        };
                        wrapped(ui, RichText::new(clip(line, 400)).monospace().size(theme::SMALL).color(color));
                    }
                });
            }
            match &run.state {
                RunState::Done(fixed) => {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.link(RichText::new(format!("Draft PR #{}", fixed.pr.number)).color(theme::teal())).clicked() {
                            let _ = open::that(&fixed.pr.html_url);
                        }
                        ui.label(RichText::new(&fixed.branch).monospace().size(theme::SMALL).color(theme::fg_dim()));
                        ui.label(
                            RichText::new(format!(
                                "{} · {} round(s){} · {} turn(s) · {} file(s) · checks: {}",
                                fixed.engine,
                                fixed.rounds,
                                fixed.reviewed_by.as_ref().map(|r| format!(" · approved by {r}")).unwrap_or_default(),
                                fixed.turns,
                                fixed.changes.len(),
                                if fixed.checks.is_empty() {
                                    "none declared".to_string()
                                } else {
                                    fixed.checks.iter().map(|c| format!("{} {}", c.name, if c.ok { "✓" } else { "✗" })).collect::<Vec<_>>().join(", ")
                                }
                            ))
                            .size(theme::SMALL)
                            .color(theme::fg_dim()),
                        );
                    });
                    for c in &fixed.changes {
                        ui.label(
                            RichText::new(format!("  {} {} +{} -{}", if c.new { "A" } else { "M" }, c.path, c.added, c.removed))
                                .monospace()
                                .size(theme::SMALL)
                                .color(theme::fg_dim()),
                        );
                    }
                    let summary_short: String = fixed.summary.lines().take(6).collect::<Vec<_>>().join("\n");
                    wrapped(ui, RichText::new(summary_short).size(theme::SMALL));
                }
                RunState::Failed(e) => {
                    ui.add_space(4.0);
                    wrapped(ui, RichText::new(e.lines().take(6).collect::<Vec<_>>().join("\n")).size(theme::SMALL).color(theme::danger()));
                    if let Some(branch) = &run.kept {
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button("Open in VS Code")
                                .on_hover_text(format!(
                                    "Checks {branch} out in a worktree under the repository's \
                                     -attempts folder and opens it in Visual Studio Code. The \
                                     Worktrees dialog lists it, with Remove, when you are done."
                                ))
                                .clicked()
                            {
                                action = CardAction::OpenAttempt(branch.clone());
                            }
                            ui.label(RichText::new(format!("kept on {branch}, not pushed")).monospace().size(theme::SMALL).color(theme::fg_dim()));
                        });
                    }
                }
                _ => {}
            }
        });
    action
}

/// The tickets and their judgements, with a checkbox each.
fn tickets(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(theme::overline("TICKETS"));
        let suggested = app.backlog.suggested().len();
        ui.label(
            RichText::new(format!(
                "{} unassigned · {suggested} the agent could take",
                app.backlog.issues.len()
            ))
            .size(theme::SMALL)
            .color(theme::fg_dim()),
        );
        ui.checkbox(&mut app.backlog.only_suggested, "Only those");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = app.backlog.selected.len();
            let can = n > 0;
            let fix = egui::Button::new(RichText::new(format!("Fix {n} selected")).strong())
                .fill(theme::ember())
                .min_size(egui::vec2(0.0, theme::CONTROL_MD));
            if ui
                .add_enabled(can, fix)
                .on_hover_text(
                    "One agent per ticket, each in its own worktree. Each ends as a draft \
                     pull request; the worktree is removed when it is done.",
                )
                .clicked()
            {
                app.backlog_fix_selected();
            }
            if suggested > 0 && ui.small_button("Select suggested").clicked() {
                app.backlog_select_suggested();
            }
            if n > 0 && ui.small_button("None").clicked() {
                app.backlog.selected.clear();
            }
        });
    });
    ui.add_space(theme::UNIT);

    let issues: Vec<BacklogIssue> = app
        .backlog
        .issues
        .iter()
        .filter(|i| !app.backlog.only_suggested || app.backlog.triage.get(&i.key).is_some_and(Triage::suggested))
        .cloned()
        .collect();
    ScrollArea::vertical().max_height(300.0).id_salt("backlog-tickets").show(ui, |ui| {
        for issue in &issues {
            ticket_row(app, ui, issue);
            ui.add_space(theme::UNIT);
        }
    });
}

fn ticket_row(app: &mut App, ui: &mut egui::Ui, issue: &BacklogIssue) {
    let triage = app.backlog.triage.get(&issue.key).cloned();
    let taken = !app.backlog.selectable(&issue.key);
    let mut ticked = app.backlog.selected.contains(&issue.key);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!taken, egui::Checkbox::without_text(&mut ticked)).changed() {
            if ticked {
                app.backlog.selected.insert(issue.key.clone());
            } else {
                app.backlog.selected.remove(&issue.key);
            }
        }
        if ui.link(RichText::new(&issue.key).monospace().strong()).on_hover_text("Open in Jira").clicked() {
            let _ = open::that(&issue.url);
        }
        ui.label(RichText::new(&issue.summary));
        let mut meta = Vec::new();
        if !issue.issue_type.is_empty() {
            meta.push(issue.issue_type.clone());
        }
        if !issue.priority.is_empty() {
            meta.push(issue.priority.clone());
        }
        if !meta.is_empty() {
            ui.label(RichText::new(meta.join(" · ")).size(theme::SMALL).color(theme::fg_dim()));
        }
        if taken {
            ui.label(RichText::new("[agent]").size(theme::SMALL).color(theme::ember()));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.add_space(22.0);
        match triage {
            Some(t) => {
                let (label, color) = if !t.in_scope {
                    ("not this repository".to_string(), theme::fg_dim())
                } else if t.suggested() {
                    (format!("I can fix this ({}%)", t.confidence), theme::add())
                } else if t.autonomous {
                    (format!("maybe ({}%)", t.confidence), theme::warn())
                } else {
                    (format!("needs a person ({}%)", t.confidence), theme::warn())
                };
                ui.label(RichText::new(label).size(theme::SMALL).color(color)).on_hover_text(&t.reason);
                if !t.area.is_empty() {
                    ui.label(RichText::new(format!("in {}/", t.area)).monospace().size(theme::SMALL).color(theme::fg_dim()));
                }
                if !t.reason.is_empty() {
                    ui.label(RichText::new(clip(&t.reason, 200)).size(theme::SMALL).color(theme::fg_dim()));
                }
            }
            None => {
                ui.label(RichText::new("not judged yet").size(theme::SMALL).color(theme::fg_dim()));
            }
        }
    });
}
