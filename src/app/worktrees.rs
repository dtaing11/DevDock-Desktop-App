//! Worktrees: the same repository checked out more than once, one branch per
//! directory.
//!
//! A branch in its own directory is how two things happen at once without
//! stashing anything: a review in one window while a feature carries on in
//! another, or — the reason this exists — a coding agent on each of several
//! branches, each in its own window with its own working tree, so none of
//! them can trip over another's half-written files.
//!
//! Everything here is plain `git worktree`. A second window is a second
//! `devdock` process started on the worktree's path; each process is one
//! repository, one agent, one terminal, which is exactly the isolation wanted.

use super::worker::{strerr, Msg};
use super::{theme, App, ConfirmAction, Dialog};
use crate::git::Worktree;
use egui::{RichText, ScrollArea};
use std::path::PathBuf;

/// Everything the worktree dialog owns.
pub struct WorktreeState {
    pub list: Vec<Worktree>,
    pub loading: bool,
    /// An add or remove is running.
    pub busy: bool,
    /// The branch for a new worktree: an existing one, or a name to create.
    pub branch: String,
    /// What a branch that does not exist yet starts from. Empty means the
    /// current `HEAD`.
    pub base: String,
    /// Where to put it. Empty means next to the main worktree, named after
    /// the repository and the branch.
    pub path: String,
    /// Open the new worktree in a second window rather than switching this
    /// one to it.
    pub new_window: bool,
    /// Why the list could not be read.
    pub error: Option<String>,
}

impl Default for WorktreeState {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            loading: false,
            busy: false,
            branch: String::new(),
            base: String::new(),
            path: String::new(),
            new_window: true,
            error: None,
        }
    }
}

impl App {
    /// Opens the worktree dialog for the current repository.
    pub fn open_worktrees(&mut self) {
        if self.repo.is_none() {
            self.dialog = Dialog::RepoPicker;
            return;
        }
        self.dialog = Dialog::Worktrees;
        self.load_worktrees();
    }

    /// Re-reads the worktree list from git.
    pub fn load_worktrees(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worktrees.loading = true;
        self.worker.spawn(move || Msg::Worktrees(strerr(repo.worktrees())));
    }

    /// Where a worktree for `branch` would go when no path is given.
    pub fn worktree_suggested_path(&self, branch: &str) -> Option<PathBuf> {
        let branch = branch.trim();
        if branch.is_empty() {
            return None;
        }
        self.repo.as_ref().map(|r| r.worktree_default_path(branch))
    }

    /// Creates the worktree described by the form, then opens it — here or
    /// in a new window.
    ///
    /// A branch that exists is checked out into the new directory (git
    /// refuses if it is already checked out somewhere, which is the right
    /// answer); one that does not is created from the base, or from `HEAD`.
    pub fn worktree_create(&mut self) {
        let branch = self.worktrees.branch.trim().to_string();
        if branch.is_empty() {
            self.toast("Name the branch for the worktree.", true);
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        let path = match self.worktrees.path.trim() {
            "" => repo.worktree_default_path(&branch),
            given => expand_home(given),
        };
        let base = self.worktrees.base.trim().to_string();
        let new_window = self.worktrees.new_window;
        self.worktrees.busy = true;
        self.worker.spawn(move || {
            let exists = repo
                .branches()
                .map(|b| b.local.iter().any(|br| br.name == branch))
                .unwrap_or(false);
            let from = if exists {
                None
            } else if base.is_empty() {
                Some("HEAD".to_string())
            } else {
                Some(base)
            };
            let result = repo.worktree_add(&path, &branch, from.as_deref()).map(|wt| {
                let verb = if exists { "checked out" } else { "created" };
                (format!("{branch} {verb} in {}", wt.path.display()), wt.path)
            });
            match result {
                Ok((message, path)) => Msg::WorktreeDone {
                    message: Ok(message),
                    open: Some((path, new_window)),
                },
                Err(e) => Msg::WorktreeDone { message: Err(e.to_string()), open: None },
            }
        });
    }

    /// Switches this window to the worktree at `path`.
    pub fn worktree_open_here(&mut self, path: &std::path::Path) {
        self.dialog = Dialog::None;
        self.open_repo(&path.display().to_string());
    }

    /// Starts another DevDock on the worktree at `path`: its own window,
    /// repository, agent, and terminal.
    pub fn worktree_open_window(&mut self, path: &std::path::Path) {
        match spawn_window(path) {
            Ok(()) => self.toast(format!("Opened {} in a new window.", path.display()), false),
            Err(e) => self.toast(format!("Could not open a new window: {e}"), true),
        }
    }

    /// Asks before removing a worktree. `force` throws away uncommitted
    /// changes in it, so it is a separate decision from removing it at all.
    pub fn worktree_remove(&mut self, path: &std::path::Path, force: bool) {
        self.confirm(ConfirmAction::RemoveWorktree {
            path: path.display().to_string(),
            force,
        });
    }

    /// Runs the removal the confirmation dialog agreed to.
    pub(super) fn worktree_remove_confirmed(&mut self, path: String, force: bool) {
        let Some(repo) = self.repo.clone() else { return };
        self.worktrees.busy = true;
        self.worker.spawn(move || {
            let result = repo
                .worktree_remove(std::path::Path::new(&path), force)
                .map(|_| format!("Removed the worktree at {path}."));
            Msg::WorktreeDone { message: strerr(result), open: None }
        });
    }

    /// Forgets worktrees whose directories are gone.
    pub fn worktree_prune(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worktrees.busy = true;
        self.worker.spawn(move || {
            let result = repo.worktree_prune().map(|_| "Pruned missing worktrees.".to_string());
            Msg::WorktreeDone { message: strerr(result), open: None }
        });
    }

    /// The worker's answer to [`Self::load_worktrees`].
    pub(super) fn on_worktrees(&mut self, result: Result<Vec<Worktree>, String>) {
        self.worktrees.loading = false;
        match result {
            Ok(list) => {
                self.worktrees.list = list;
                self.worktrees.error = None;
            }
            Err(e) => self.worktrees.error = Some(e),
        }
    }

    /// The worker's answer to an add or remove.
    pub(super) fn on_worktree_done(
        &mut self,
        message: Result<String, String>,
        open: Option<(PathBuf, bool)>,
    ) {
        self.worktrees.busy = false;
        match message {
            Ok(m) => self.toast(m, false),
            Err(e) => self.toast(e, true),
        }
        if let Some((path, new_window)) = open {
            self.worktrees.branch.clear();
            self.worktrees.base.clear();
            self.worktrees.path.clear();
            if new_window {
                self.worktree_open_window(&path);
            } else {
                self.worktree_open_here(&path);
            }
        }
        self.load_worktrees();
        self.refresh();
    }
}

/// `~/x` as the shell would read it.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Starts another instance of this executable on `path`.
///
/// `devdock <path>` opens the app on that repository, so the new window is
/// a new process: its own repository, agent, and terminal, sharing nothing
/// with this one but the config file.
pub fn spawn_window(path: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The dialog: every checkout of the repository, and a form for one more.
pub fn dialog(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    super::dialogs::modal(ctx, "Worktrees", open, |ui| {
        ui.set_min_width(640.0);
        ui.label(
            RichText::new(
                "The same repository checked out more than once, one branch per \
                 directory. Work on two branches at once — or run a coding agent on \
                 each — without stashing anything.",
            )
            .color(theme::fg_dim())
            .small(),
        );
        ui.add_space(8.0);

        if let Some(error) = app.worktrees.error.clone() {
            ui.label(RichText::new(error).color(theme::danger()));
            ui.add_space(6.0);
        }

        let here = app.repo.as_ref().map(|r| r.path().to_path_buf());
        let busy = app.worktrees.busy;
        let list = app.worktrees.list.clone();

        if list.is_empty() && app.worktrees.loading {
            ui.label(RichText::new("Reading worktrees…").color(theme::fg_dim()));
        }
        ScrollArea::vertical().max_height(300.0).id_salt("worktrees").show(ui, |ui| {
            for wt in &list {
                let current = here.as_deref() == Some(wt.path.as_path());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if current { "●" } else { "○" })
                            .color(if current { theme::ember() } else { theme::border() })
                            .monospace(),
                    );
                    let name = wt.branch.clone().unwrap_or_else(|| {
                        format!("(detached at {})", &wt.head[..wt.head.len().min(7)])
                    });
                    ui.label(
                        RichText::new(name)
                            .color(if current { theme::ember() } else { theme::fg() })
                            .monospace()
                            .strong(),
                    );
                    if wt.main {
                        ui.label(RichText::new("main worktree").color(theme::fg_dim()).small());
                    }
                    if current {
                        ui.label(RichText::new("this window").color(theme::fg_dim()).small());
                    }
                    if wt.locked {
                        ui.label(RichText::new("[locked]").color(theme::warn()).small());
                    }
                    if wt.prunable {
                        ui.label(RichText::new("[missing]").color(theme::danger()).small())
                            .on_hover_text("Its directory is gone. Prune forgets it.");
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new(wt.path.display().to_string())
                            .color(theme::fg_dim())
                            .small(),
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    if !current
                        && !wt.prunable
                        && ui
                            .add_enabled(!busy, egui::Button::new("Switch here").small())
                            .on_hover_text("Open it in this window.")
                            .clicked()
                    {
                        app.worktree_open_here(&wt.path);
                    }
                    if !wt.prunable
                        && ui
                            .add_enabled(!busy, egui::Button::new("New window").small())
                            .on_hover_text(
                                "Start another DevDock on it: its own agent and terminal.",
                            )
                            .clicked()
                    {
                        app.worktree_open_window(&wt.path);
                    }
                    if !wt.main && !current {
                        if ui
                            .add_enabled(!busy, egui::Button::new("Remove").small())
                            .on_hover_text(
                                "Deletes the directory. Refused if it has uncommitted \
                                 changes; the branch is kept either way.",
                            )
                            .clicked()
                        {
                            app.worktree_remove(&wt.path, false);
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("Remove, discarding").small())
                            .on_hover_text("Deletes the directory even with uncommitted changes.")
                            .clicked()
                        {
                            app.worktree_remove(&wt.path, true);
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });

        if list.iter().any(|w| w.prunable)
            && ui.add_enabled(!busy, egui::Button::new("Prune missing").small()).clicked()
        {
            app.worktree_prune();
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        ui.label(RichText::new("New worktree").strong());

        let locals: Vec<String> = app
            .branches
            .as_ref()
            .map(|b| b.local.iter().map(|br| br.name.clone()).collect())
            .unwrap_or_default();
        let checked_out: Vec<String> =
            list.iter().filter_map(|w| w.branch.clone()).collect();

        ui.horizontal(|ui| {
            ui.label("Branch");
            ui.add(
                egui::TextEdit::singleline(&mut app.worktrees.branch)
                    .hint_text("existing branch, or a name to create")
                    .desired_width(240.0),
            );
            egui::ComboBox::from_id_salt("worktree-branch-pick")
                .selected_text("existing…")
                .width(120.0)
                .show_ui(ui, |ui| {
                    for name in &locals {
                        let elsewhere = checked_out.contains(name);
                        let label = if elsewhere {
                            format!("{name}  (checked out)")
                        } else {
                            name.clone()
                        };
                        if ui.add_enabled(!elsewhere, egui::Button::new(label)).clicked() {
                            app.worktrees.branch = name.clone();
                        }
                    }
                });
        });
        let branch = app.worktrees.branch.trim().to_string();
        let is_new = !branch.is_empty() && !locals.contains(&branch);
        if is_new {
            ui.horizontal(|ui| {
                ui.label("Start from");
                ui.add(
                    egui::TextEdit::singleline(&mut app.worktrees.base)
                        .hint_text("current HEAD")
                        .desired_width(240.0),
                );
                ui.label(
                    RichText::new(format!("{branch} does not exist yet; it will be created."))
                        .color(theme::fg_dim())
                        .small(),
                );
            });
        }
        let suggested = app
            .worktree_suggested_path(&branch)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "next to the main worktree".to_string());
        ui.horizontal(|ui| {
            ui.label("Directory");
            ui.add(
                egui::TextEdit::singleline(&mut app.worktrees.path)
                    .hint_text(suggested)
                    .desired_width(380.0),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.worktrees.new_window, "Open in a new window").on_hover_text(
                "A second DevDock on the new worktree, with its own agent and terminal. \
                 Unticked, this window switches to it.",
            );
            let create = egui::Button::new(RichText::new("Create worktree").strong())
                .fill(theme::ember());
            if ui.add_enabled(!busy && !branch.is_empty(), create).clicked() {
                app.worktree_create();
            }
            if busy {
                ui.spinner();
            }
        });
    });
}
