//! Every agent run the window knows of, in one place: the runs in the
//! repository on screen and in every repository kept aside — worktree
//! runs, backlog tickets, and the run in each tree — each a card that
//! unfolds to its log, with the same buttons the Agent tab and the
//! backlog give it: answer its question, open the kept attempt in Visual
//! Studio Code, open it as a pull request. Switching repositories is not
//! needed to see or act on any of them.

use eframe::egui::{self, RichText, ScrollArea};

use super::agent_tab::CodingState;
use super::backlog::{run_card, BacklogState, CardAction, RunState, TicketRun};
use super::{theme, App, Tab};

/// What the tab keeps between frames.
#[derive(Default)]
pub struct RunsView {
    /// `<repository>\n<run>` of the card whose log is unfolded.
    pub expanded: Option<String>,
    pub filter: Filter,
}

/// Which runs to show.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Running,
    /// Waiting on an answer from you.
    Waiting,
    Failed,
    Done,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::All, Filter::Running, Filter::Waiting, Filter::Failed, Filter::Done];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Running => "Running",
            Filter::Waiting => "Waiting on you",
            Filter::Failed => "Failed",
            Filter::Done => "Done",
        }
    }

    pub fn admits(self, run: &TicketRun) -> bool {
        match self {
            Filter::All => true,
            Filter::Running => run.is_running() || matches!(run.state, RunState::Queued),
            Filter::Waiting => run.question.is_some(),
            Filter::Failed => matches!(run.state, RunState::Failed(_)),
            Filter::Done => matches!(run.state, RunState::Done(_)),
        }
    }

    /// The same, for the run in a tree, which is not a [`TicketRun`].
    pub fn admits_tree(self, coding: &CodingState) -> bool {
        match self {
            Filter::All => true,
            Filter::Running => coding.running,
            Filter::Waiting => coding.question.is_some(),
            Filter::Failed => !coding.running && coding.error.is_some(),
            Filter::Done => !coding.running && coding.error.is_none(),
        }
    }
}

/// How many runs a repository has in each state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub running: usize,
    pub waiting: usize,
    pub failed: usize,
    pub done: usize,
}

impl Tally {
    fn add(&mut self, run: &TicketRun) {
        if run.question.is_some() {
            self.waiting += 1;
        }
        match run.state {
            RunState::Queued | RunState::Running => self.running += 1,
            RunState::Done(_) => self.done += 1,
            RunState::Failed(_) => self.failed += 1,
        }
    }

    fn add_tree(&mut self, coding: &CodingState) {
        if coding.running {
            self.running += 1;
        }
        if coding.question.is_some() {
            self.waiting += 1;
        }
        if !coding.running && !coding.log.is_empty() {
            if coding.error.is_some() {
                self.failed += 1;
            } else {
                self.done += 1;
            }
        }
    }

    pub fn total(&self) -> usize {
        self.running + self.failed + self.done
    }

    /// The counts as one line: what is non-zero, in the order it matters.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if self.running > 0 {
            parts.push(format!("{} running", self.running));
        }
        if self.waiting > 0 {
            parts.push(format!("{} waiting on you", self.waiting));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.done > 0 {
            parts.push(format!("{} done", self.done));
        }
        if parts.is_empty() {
            "no runs".into()
        } else {
            parts.join(" · ")
        }
    }
}

/// One repository's runs and where they live.
struct Source<'a> {
    key: String,
    name: String,
    current: bool,
    coding: &'a mut CodingState,
    backlog: &'a mut BacklogState,
}

/// The repository's name for a heading: the last path segment.
fn name_of(key: &str) -> String {
    std::path::Path::new(key).file_name().and_then(|n| n.to_str()).unwrap_or(key).to_string()
}

fn tally_of(coding: &CodingState, backlog: &BacklogState) -> Tally {
    let mut tally = Tally::default();
    tally.add_tree(coding);
    for run in coding.worktree.runs.values().chain(backlog.runs.values()) {
        tally.add(run);
    }
    tally
}

/// Every repository with its tally, the one on screen first, the rest by
/// path. For the sidebar and the tab's label.
pub fn tallies(app: &App) -> Vec<(String, String, Tally)> {
    let current = app.repo_key();
    let mut out = Vec::new();
    if !current.is_empty() {
        out.push((current.clone(), name_of(&current), tally_of(&app.coding, &app.backlog)));
    }
    let mut rest: Vec<(&String, &super::RepoSession)> = app.sessions.iter().filter(|(k, _)| **k != current).collect();
    rest.sort_by(|a, b| a.0.cmp(b.0));
    for (key, session) in rest {
        out.push((key.clone(), name_of(key), tally_of(&session.coding, &session.backlog)));
    }
    out
}

/// Agents at work across every repository, for the tab's label.
pub fn running_everywhere(app: &App) -> usize {
    tallies(app).iter().map(|(_, _, t)| t.running).sum()
}

/// What a card asked for, done once the borrows of the runs are over.
enum Pending {
    OpenAttempt { repo: String, branch: String },
    PublishAttempt { repo: String, branch: String },
    /// Show the repository's Agent tab.
    GoTo { repo: String },
}

/// The sidebar: the filter, and each repository's counts.
pub fn runs_sidebar(app: &mut App, ui: &mut egui::Ui) {
    ui.label(theme::overline("SHOW"));
    for filter in Filter::ALL {
        if ui.selectable_label(app.runs_view.filter == filter, filter.label()).clicked() {
            app.runs_view.filter = filter;
        }
    }
    ui.add_space(theme::UNIT * 2.0);
    ui.label(theme::overline("REPOSITORIES"));
    let tallies = tallies(app);
    if tallies.is_empty() {
        ui.label(RichText::new("Open a repository to see its agents here.").small().color(theme::fg_dim()));
    }
    let mut finished = 0;
    for (key, name, tally) in &tallies {
        finished += tally.failed + tally.done;
        ui.horizontal(|ui| {
            if *key == app.repo_key() {
                ui.label(RichText::new(name).font(theme::semibold(theme::TEXT)));
            } else {
                ui.label(RichText::new(name).color(theme::fg_dim()));
            }
            if tally.running > 0 {
                ui.add(egui::Spinner::new().size(theme::SPINNER));
            }
        });
        ui.label(RichText::new(tally.line()).small().color(theme::fg_dim()));
        ui.add_space(theme::UNIT);
    }
    if finished > 0 && ui.small_button("Clear finished everywhere").on_hover_text("Drops every done and failed card, in every repository. Kept attempts stay on their branches.").clicked() {
        clear_finished(app);
    }
    ui.add_space(theme::UNIT * 2.0);
    ui.label(
        RichText::new(
            "Every agent run, in this repository and in every one kept aside. \
             A card unfolds to its log; a failed one opens its kept attempt in \
             Visual Studio Code or as a pull request; one with a question takes \
             your answer here.",
        )
        .small()
        .color(theme::fg_dim()),
    );
}

fn clear_finished(app: &mut App) {
    app.coding.worktree.clear_finished();
    app.backlog.runs.retain(|_, r| !r.is_finished());
    for session in app.sessions.values_mut() {
        session.coding.worktree.clear_finished();
        session.backlog.runs.retain(|_, r| !r.is_finished());
    }
}

/// The viewport: every repository's runs, cards that unfold.
pub fn runs_viewport(app: &mut App, ui: &mut egui::Ui) {
    let current = app.repo_key();
    let mut pending: Vec<Pending> = Vec::new();
    {
        let App { coding, backlog, sessions, screenshots, runs_view, .. } = app;
        let mut sources: Vec<Source<'_>> = Vec::new();
        if !current.is_empty() {
            sources.push(Source { key: current.clone(), name: name_of(&current), current: true, coding, backlog });
        }
        let mut rest: Vec<(&String, &mut super::RepoSession)> = sessions.iter_mut().filter(|(k, _)| **k != current).collect();
        rest.sort_by(|a, b| a.0.cmp(b.0));
        for (key, session) in rest {
            sources.push(Source { key: key.clone(), name: name_of(key), current: false, coding: &mut session.coding, backlog: &mut session.backlog });
        }

        let any_running = sources.iter().any(|s| s.coding.running || s.coding.worktree.running() > 0 || s.backlog.running() > 0);
        if any_running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
        if sources.is_empty() {
            ui.label(RichText::new("Open a repository to see its agents here.").color(theme::fg_dim()));
            return;
        }
        ScrollArea::vertical().auto_shrink([false, false]).id_salt("runs-everywhere").show(ui, |ui| {
            let mut shown = 0;
            for source in sources.iter_mut() {
                shown += repository_section(ui, source, runs_view, screenshots, &mut pending);
            }
            if shown == 0 {
                ui.add_space(theme::UNIT * 2.0);
                ui.label(
                    RichText::new(match runs_view.filter {
                        Filter::All => "No agent has run yet. Start one from the Agent tab or the backlog.",
                        Filter::Running => "Nothing is running.",
                        Filter::Waiting => "No run is waiting on you.",
                        Filter::Failed => "Nothing has failed.",
                        Filter::Done => "Nothing has finished yet.",
                    })
                    .color(theme::fg_dim()),
                );
            }
        });
    }
    for action in pending {
        match action {
            Pending::OpenAttempt { repo, branch } => app.open_attempt_in_vscode_of(&repo, &branch),
            Pending::PublishAttempt { repo, branch } => app.publish_attempt_of(&repo, &branch),
            Pending::GoTo { repo } => {
                if repo != app.repo_key() {
                    app.open_repo(&repo);
                }
                app.tab = Tab::Agent;
            }
        }
    }
}

/// One repository: its heading and its cards. How many cards were shown.
fn repository_section(
    ui: &mut egui::Ui,
    source: &mut Source<'_>,
    view: &mut RunsView,
    shots: &mut super::backlog::Screenshots,
    pending: &mut Vec<Pending>,
) -> usize {
    let filter = view.filter;
    let tree_shown = (source.coding.running || !source.coding.log.is_empty()) && filter.admits_tree(source.coding);
    let worktree_keys: Vec<String> = source.coding.worktree.runs.iter().filter(|(_, r)| filter.admits(r)).map(|(k, _)| k.clone()).collect();
    let backlog_keys: Vec<String> = source.backlog.runs.iter().filter(|(_, r)| filter.admits(r)).map(|(k, _)| k.clone()).collect();
    let count = usize::from(tree_shown) + worktree_keys.len() + backlog_keys.len();
    if count == 0 {
        return 0;
    }

    let tally = tally_of(source.coding, source.backlog);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&source.name).font(theme::semibold(theme::SUBTITLE)));
        if source.current {
            ui.label(RichText::new("on screen").small().color(theme::teal()));
        } else if ui.small_button("Show").on_hover_text(format!("Switch the window to {}", source.key)).clicked() {
            pending.push(Pending::GoTo { repo: source.key.clone() });
        }
        ui.label(RichText::new(tally.line()).small().color(theme::fg_dim()));
    });
    ui.label(RichText::new(&source.key).small().color(theme::fg_dim()));
    ui.add_space(theme::UNIT);

    if tree_shown {
        let id = format!("{}\ntree", source.key);
        let expanded = view.expanded.as_deref() == Some(id.as_str());
        match tree_card(ui, source, expanded) {
            TreeAction::None | TreeAction::Answer => {}
            TreeAction::ToggleLog => view.expanded = if expanded { None } else { Some(id) },
            TreeAction::GoTo => pending.push(Pending::GoTo { repo: source.key.clone() }),
        }
        ui.add_space(theme::UNIT);
    }

    for (keys, runs, what) in [
        (worktree_keys, &mut source.coding.worktree.runs, "worktree"),
        (backlog_keys, &mut source.backlog.runs, "ticket"),
    ] {
        for key in keys {
            let id = format!("{}\n{what}\n{key}", source.key);
            let expanded = view.expanded.as_deref() == Some(id.as_str());
            let Some(run) = runs.get_mut(&key) else { continue };
            let title = format!("{what} · {}", run.title);
            let salt = format!("runs-{}-{what}", source.key);
            match run_card(ui, &key, &title, run, expanded, &salt, shots) {
                CardAction::None => {}
                CardAction::ToggleLog => view.expanded = if expanded { None } else { Some(id) },
                CardAction::OpenAttempt(branch) => pending.push(Pending::OpenAttempt { repo: source.key.clone(), branch }),
                CardAction::PublishAttempt(branch) => pending.push(Pending::PublishAttempt { repo: source.key.clone(), branch }),
                CardAction::Answer => {
                    if let Some(q) = runs.get_mut(&key).and_then(|r| r.question.take()) {
                        q.answer();
                    }
                }
            }
            ui.add_space(theme::UNIT);
        }
    }
    ui.add_space(theme::UNIT * 2.0);
    count
}

enum TreeAction {
    None,
    ToggleLog,
    GoTo,
    Answer,
}

/// The run in the repository's own tree: the Agent tab's, which is not a
/// worktree run and has no card of its own there. Its state, its last
/// line, its log on request, its question when it has one.
fn tree_card(ui: &mut egui::Ui, source: &mut Source<'_>, expanded: bool) -> TreeAction {
    let coding = &mut *source.coding;
    let (state_label, color) = if coding.running {
        ("working", theme::ember())
    } else if coding.error.is_some() {
        ("failed", theme::danger())
    } else {
        ("finished", theme::add())
    };
    let title = coding
        .history
        .last()
        .map(|e| e.task.clone())
        .filter(|_| !coding.running)
        .unwrap_or_else(|| coding.task.clone());
    let title = title.lines().next().unwrap_or("(no task)").chars().take(90).collect::<String>();
    let last = coding.log.last().cloned().unwrap_or_default();
    let mut action = TreeAction::None;
    let width = ui.available_width();
    egui::Frame::new()
        .fill(theme::panel())
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(width - 20.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(state_label).small().color(color));
                if coding.running {
                    ui.add(egui::Spinner::new().size(theme::SPINNER));
                }
                ui.label(RichText::new("in this tree").small().color(theme::fg_dim()));
                ui.label(RichText::new(&title).font(theme::semibold(theme::TEXT)));
                if let Some(took) = coding.took.filter(|_| !coding.running) {
                    ui.label(RichText::new(super::backlog::mmss(took)).small().color(theme::fg_dim()));
                }
            });
            if !last.is_empty() && !expanded {
                super::backlog::wrapped(ui, RichText::new(last.chars().take(200).collect::<String>()).size(theme::SMALL).color(theme::fg_dim()));
            }
            ui.horizontal_wrapped(|ui| {
                if ui.small_button(if expanded { "Hide log" } else { "Log" }).clicked() {
                    action = TreeAction::ToggleLog;
                }
                if ui.small_button("Agent tab").on_hover_text("The full view: the diff, the plan, the transcript").clicked() {
                    action = TreeAction::GoTo;
                }
            });
            if let Some(q) = coding.question.as_mut() {
                ui.add_space(theme::UNIT);
                egui::Frame::new()
                    .fill(theme::ember().linear_multiply(0.12))
                    .stroke(egui::Stroke::new(1.0_f32, theme::ember()))
                    .corner_radius(theme::RADIUS_MD as f32)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.label(RichText::new("The agent asks:").font(theme::semibold(theme::TEXT)).color(theme::ember()));
                        ui.add(egui::Label::new(RichText::new(&q.question).color(theme::fg())).wrap());
                        super::views::prose_box(ui, &mut q.draft, 2, "Your answer — it is waiting");
                        let ready = !q.draft.trim().is_empty();
                        if ui.add_enabled(ready, egui::Button::new("Answer").fill(theme::ember())).clicked() {
                            action = TreeAction::Answer;
                        }
                    });
            }
            if expanded {
                ui.add_space(theme::UNIT);
                if let Some(error) = &coding.error {
                    super::backlog::wrapped(ui, RichText::new(error.lines().take(6).collect::<Vec<_>>().join("\n")).size(theme::SMALL).color(theme::danger()));
                }
                if !coding.summary.is_empty() {
                    super::backlog::wrapped(ui, RichText::new(coding.summary.lines().take(8).collect::<Vec<_>>().join("\n")).size(theme::SMALL));
                }
                ScrollArea::vertical().max_height(260.0).id_salt(format!("runs-tree-log-{}", source.key)).stick_to_bottom(true).show(ui, |ui| {
                    for line in coding.log.iter().rev().take(400).collect::<Vec<_>>().into_iter().rev() {
                        ui.label(RichText::new(line).monospace().size(theme::SMALL).color(theme::fg_dim()));
                    }
                });
            }
        });
    // The answer: sent from here, the same as the Agent tab would.
    if matches!(action, TreeAction::Answer) {
        if let Some(q) = coding.question.take() {
            q.answer();
        }
        return TreeAction::None;
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::backlog::PendingQuestion;

    fn run(state: RunState, question: bool) -> TicketRun {
        let mut run = TicketRun::queued("t");
        run.state = state;
        if question {
            let (tx, _rx) = std::sync::mpsc::channel();
            run.question = Some(PendingQuestion { question: "which?".into(), draft: String::new(), reply: tx });
        }
        run
    }

    #[test]
    fn filters_and_tallies_read_every_state() {
        let running = run(RunState::Running, false);
        let waiting = run(RunState::Running, true);
        let failed = run(RunState::Failed("x".into()), false);
        assert!(Filter::All.admits(&failed) && Filter::Running.admits(&running) && !Filter::Running.admits(&failed));
        assert!(Filter::Waiting.admits(&waiting) && !Filter::Waiting.admits(&running));
        assert!(Filter::Failed.admits(&failed) && !Filter::Done.admits(&failed));

        let mut tally = Tally::default();
        for r in [&running, &waiting, &failed] {
            tally.add(r);
        }
        assert_eq!(tally, Tally { running: 2, waiting: 1, failed: 1, done: 0 });
        assert_eq!(tally.line(), "2 running · 1 waiting on you · 1 failed");
        assert_eq!(Tally::default().line(), "no runs");

        let mut coding = CodingState::default();
        assert!(!Filter::Running.admits_tree(&coding));
        coding.running = true;
        assert!(Filter::Running.admits_tree(&coding) && !Filter::Failed.admits_tree(&coding));
        let mut t = Tally::default();
        t.add_tree(&coding);
        assert_eq!(t.running, 1);
    }

    /// Two repositories, one on screen and one kept aside: both are listed,
    /// the one on screen first, with each one's own counts.
    #[test]
    fn every_repository_is_tallied_with_the_one_on_screen_first() {
        let ctx = egui::Context::default();
        let mut app = App::new_for_test(&ctx);
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            let out = std::process::Command::new("git").args(["init", "-q"]).current_dir(dir).output().unwrap();
            assert!(out.status.success());
        }
        app.repo = Some(crate::git::Repo::open(&b).unwrap());
        app.coding.worktree.runs.insert("agent/b".into(), run(RunState::Running, false));

        let mut session = super::super::RepoSession::default();
        session.backlog.runs.insert("ABC-1".into(), run(RunState::Failed("no".into()), false));
        session.backlog.runs.insert("ABC-2".into(), run(RunState::Running, true));
        app.sessions.insert(a.display().to_string(), session);

        let tallies = tallies(&app);
        assert_eq!(tallies.len(), 2);
        assert_eq!(tallies[0].1, "b");
        assert_eq!(tallies[0].2, Tally { running: 1, waiting: 0, failed: 0, done: 0 });
        assert_eq!(tallies[1].1, "a");
        assert_eq!(tallies[1].2, Tally { running: 1, waiting: 1, failed: 1, done: 0 });
        assert_eq!(running_everywhere(&app), 2);

        clear_finished(&mut app);
        assert_eq!(app.sessions[&a.display().to_string()].backlog.runs.len(), 1, "the failed one is gone, the running one stays");
    }
}
