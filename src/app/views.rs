//! Main panels: toolbar, sidebar (changes/history), diff view, toasts.

use super::theme;
use super::worker::{pickable_branches, strerr, Msg};
use super::{App, Dialog, Tab};
use crate::git::{FileStatus, RepoState};
use egui::{Color32, RichText, ScrollArea};

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

/// Top toolbar: repo/branch pickers, sync buttons, merge/rebase, GitHub.
pub fn toolbar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("toolbar")
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(10.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                repo_button(app, ui);
                branch_menu(app, ui);
                sync_buttons(app, ui);
                merge_rebase_menus(app, ui);
                if ui.button("⇄ Pull Request").clicked() {
                    open_pr_dialog(app);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").on_hover_text("Settings").clicked() {
                        app.dialog = Dialog::Settings;
                    }
                    let gh_label = app
                        .gh
                        .user
                        .as_ref()
                        .map(|u| u.login.clone())
                        .unwrap_or_else(|| "Sign in".into());
                    if ui.button(format!("🐙 {gh_label}")).on_hover_text("GitHub").clicked() {
                        app.dialog = Dialog::GitHub;
                    }
                });
            });
            state_banner(app, ui);
        });
}

fn repo_button(app: &mut App, ui: &mut egui::Ui) {
    let name = app
        .repo
        .as_ref()
        .map(|r| r.name())
        .unwrap_or_else(|| "Open repository…".into());
    let text = RichText::new(format!("📁 {name}")).strong();
    if ui.button(text).on_hover_text("Change repository").clicked() {
        app.dialog = Dialog::RepoPicker;
    }
}

fn branch_menu(app: &mut App, ui: &mut egui::Ui) {
    let current = app
        .status
        .as_ref()
        .map(|s| s.branch.clone())
        .unwrap_or_else(|| "—".into());
    let label = RichText::new(format!("⑂ {current}")).strong();

    ui.menu_button(label, |ui| {
        ui.set_min_width(300.0);
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut app.new_branch_name);
            if ui.button("＋ Create").clicked() && !app.new_branch_name.trim().is_empty() {
                if let Some(repo) = app.repo.clone() {
                    let name = app.new_branch_name.trim().to_string();
                    app.new_branch_name.clear();
                    app.worker.spawn(move || Msg::Done {
                        message: strerr(
                            repo.create_branch(&name, true)
                                .map(|_| format!("Switched to new branch {name}")),
                        ),
                        refresh: true,
                    });
                }
                ui.close_menu();
            }
        });
        ui.separator();
        ui.text_edit_singleline(&mut app.branch_filter);
        let filter = app.branch_filter.to_lowercase();

        let (locals, remotes) = app
            .branches
            .as_ref()
            .map(|b| (b.local.clone(), b.remote.clone()))
            .unwrap_or_default();

        ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            ui.label(RichText::new("LOCAL").color(theme::EMBER).small());
            for branch in locals.iter().filter(|b| b.name.to_lowercase().contains(&filter)) {
                let marker = if branch.current { "✔ " } else { "" };
                if ui.button(format!("{marker}{}", branch.name)).clicked() {
                    checkout(app, &branch.name);
                    ui.close_menu();
                }
            }
            ui.label(RichText::new("REMOTE").color(theme::EMBER).small());
            for branch in remotes.iter().filter(|b| b.name.to_lowercase().contains(&filter)) {
                if ui.button(&branch.name).clicked() {
                    // Prefer creating a local tracking branch.
                    let local =
                        branch.name.split_once('/').map(|(_, l)| l).unwrap_or(&branch.name);
                    checkout(app, local);
                    ui.close_menu();
                }
            }
        });
    });
}

fn checkout(app: &mut App, name: &str) {
    let Some(repo) = app.repo.clone() else { return };
    let name = name.to_string();
    app.worker.spawn(move || Msg::Done {
        message: strerr(repo.checkout(&name).map(|_| format!("Switched to {name}"))),
        refresh: true,
    });
}

fn sync_buttons(app: &mut App, ui: &mut egui::Ui) {
    let (ahead, behind, has_upstream) = app
        .status
        .as_ref()
        .map(|s| (s.ahead, s.behind, s.has_upstream))
        .unwrap_or((0, 0, false));

    if ui.button("⟳ Fetch").clicked() {
        run_sync(app, "fetch");
    }
    let pull_label = if behind > 0 { format!("⇣ Pull ({behind})") } else { "⇣ Pull".into() };
    if ui.button(pull_label).clicked() {
        run_sync(app, "pull");
    }
    let push_label = if !has_upstream {
        "⇡ Publish".to_string()
    } else if ahead > 0 {
        format!("⇡ Push ({ahead})")
    } else {
        "⇡ Push".to_string()
    };
    if ui.button(push_label).clicked() {
        run_sync(app, "push");
    }
}

fn run_sync(app: &mut App, action: &'static str) {
    let Some(repo) = app.repo.clone() else {
        app.dialog = Dialog::RepoPicker;
        return;
    };
    let token = app.gh_token();
    let set_upstream = !app.status.as_ref().map(|s| s.has_upstream).unwrap_or(false);
    app.busy = true;
    app.worker.spawn(move || {
        let auth = token.as_deref();
        let result = match action {
            "fetch" => repo.fetch(auth).map(|_| "Fetched.".to_string()),
            "pull" => repo.pull(auth).map(|out| {
                out.lines().last().unwrap_or("Pulled.").to_string()
            }),
            "push" => repo.push(set_upstream, auth).map(|_| "Pushed.".to_string()),
            _ => unreachable!(),
        };
        Msg::Done { message: strerr(result), refresh: true }
    });
}

fn merge_rebase_menus(app: &mut App, ui: &mut egui::Ui) {
    let branches =
        app.branches.as_ref().map(pickable_branches).unwrap_or_default();

    ui.menu_button("⑃ Merge", |ui| {
        ui.set_min_width(240.0);
        ui.label(RichText::new("Merge into current branch").color(theme::FG_DIM).small());
        for branch in &branches {
            if ui.button(&branch.name).clicked() {
                let repo = app.repo.clone();
                let name = branch.name.clone();
                if let Some(repo) = repo {
                    app.busy = true;
                    app.worker.spawn(move || Msg::MergeOutcome(repo.merge(&name)));
                }
                ui.close_menu();
            }
        }
    });

    ui.menu_button("⤴ Rebase", |ui| {
        ui.set_min_width(240.0);
        ui.label(RichText::new("Rebase current branch onto").color(theme::FG_DIM).small());
        for branch in &branches {
            if ui.button(&branch.name).clicked() {
                let repo = app.repo.clone();
                let name = branch.name.clone();
                if let Some(repo) = repo {
                    app.busy = true;
                    app.worker.spawn(move || Msg::MergeOutcome(repo.rebase(&name)));
                }
                ui.close_menu();
            }
        }
    });
}

fn open_pr_dialog(app: &mut App) {
    if app.repo.is_none() {
        app.dialog = Dialog::RepoPicker;
        return;
    }
    if app.gh.user.is_none() {
        app.toast("Sign in to GitHub first.", true);
        app.dialog = Dialog::GitHub;
        return;
    }
    app.pr.head = app.status.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
    app.pr.base = app
        .branches
        .as_ref()
        .and_then(|b| {
            b.local
                .iter()
                .find(|br| !br.current && (br.name == "main" || br.name == "master"))
                .map(|br| br.name.clone())
        })
        .unwrap_or_else(|| "main".into());
    app.pr.title.clear();
    app.pr.body.clear();
    app.pr.open_prs.clear();
    app.pr.loading = true;
    app.dialog = Dialog::PullRequests;

    let repo = app.repo.clone().unwrap();
    app.worker.spawn(move || {
        let result = (|| -> Result<Vec<crate::github::PullRequest>, String> {
            let client = crate::github::Client::from_store().ok_or("Not signed in")?;
            let slug = origin_slug(&repo).ok_or("No github.com remote found")?;
            strerr(client.pull_requests(&slug))
        })();
        Msg::GhPrs(result)
    });
}

/// The GitHub slug of `origin` (or the first github.com remote).
pub fn origin_slug(repo: &crate::git::Repo) -> Option<crate::github::RepoSlug> {
    let remotes = repo.remotes().ok()?;
    remotes
        .iter()
        .find(|r| r.name == "origin")
        .or_else(|| remotes.first())
        .and_then(|r| crate::github::parse_remote(&r.url))
}

fn state_banner(app: &mut App, ui: &mut egui::Ui) {
    let Some(state) = app.status.as_ref().map(|s| s.state) else { return };
    if state == RepoState::Clean {
        return;
    }
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::EMBER_DEEP.linear_multiply(0.25))
        .stroke(egui::Stroke::new(1.0_f32, theme::EMBER_DEEP))
        .corner_radius(8.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let text = match state {
                    RepoState::Merging => "Merge in progress.",
                    RepoState::Rebasing => "Rebase in progress.",
                    RepoState::CherryPicking => "Cherry-pick in progress.",
                    RepoState::Clean => unreachable!(),
                };
                ui.label(RichText::new(text).strong());
                if ui.button("Resolve conflicts").clicked() {
                    app.load_conflicts();
                }
                if state == RepoState::Rebasing && ui.button("Continue").clicked() {
                    if let Some(repo) = app.repo.clone() {
                        app.worker.spawn(move || Msg::MergeOutcome(repo.rebase_continue()));
                    }
                }
                if ui.button("Abort").clicked() {
                    if let Some(repo) = app.repo.clone() {
                        app.worker.spawn(move || {
                            let result = match state {
                                RepoState::Merging => repo.merge_abort(),
                                _ => repo.rebase_abort(),
                            };
                            Msg::Done {
                                message: strerr(result.map(|_| "Aborted.".to_string())),
                                refresh: true,
                            }
                        });
                    }
                }
            });
        });
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

/// Left sidebar with the Changes and History tabs.
pub fn sidebar(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::left("sidebar")
        .default_width(340.0)
        .min_width(280.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(8.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let changes_label = format!(
                    "Changes ({})",
                    app.status.as_ref().map(|s| s.files.len()).unwrap_or(0)
                );
                if ui.selectable_label(app.tab == Tab::Changes, changes_label).clicked() {
                    app.tab = Tab::Changes;
                    app.refresh();
                }
                if ui.selectable_label(app.tab == Tab::History, "History").clicked() {
                    app.tab = Tab::History;
                    app.refresh();
                }
            });
            ui.separator();
            match app.tab {
                Tab::Changes => changes_tab(app, ui),
                Tab::History => history_tab(app, ui),
            }
        });
}

fn status_glyph(status: Option<FileStatus>, conflicted: bool) -> (&'static str, Color32) {
    if conflicted {
        return ("!", theme::DANGER);
    }
    match status {
        Some(FileStatus::Modified) => ("M", theme::WARN),
        Some(FileStatus::Added) | Some(FileStatus::Untracked) => ("A", theme::ADD),
        Some(FileStatus::Deleted) => ("D", theme::DEL),
        Some(FileStatus::Renamed) => ("R", theme::TEAL),
        Some(FileStatus::Copied) => ("C", theme::TEAL),
        Some(FileStatus::Typechange) => ("T", theme::WARN),
        _ => ("·", theme::FG_DIM),
    }
}

fn changes_tab(app: &mut App, ui: &mut egui::Ui) {
    let files = app.status.as_ref().map(|s| s.files.clone()).unwrap_or_default();

    // File list fills the space above the commit box.
    let commit_box_height = 190.0;
    let list_height = (ui.available_height() - commit_box_height).max(60.0);
    ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(
        ui,
        |ui| {
            if files.is_empty() {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No local changes").color(theme::FG_DIM));
                });
            }
            for file in &files {
                let mut checked = !app.unchecked.contains(&file.path);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut checked, "").changed() {
                        if checked {
                            app.unchecked.remove(&file.path);
                        } else {
                            app.unchecked.insert(file.path.clone());
                        }
                    }
                    let (glyph, color) = status_glyph(
                        file.work_status.or(file.index_status),
                        file.conflicted,
                    );
                    ui.label(RichText::new(glyph).color(color).strong().monospace());
                    let selected = app.selected_file.as_deref() == Some(&file.path);
                    let display = file
                        .orig_path
                        .as_ref()
                        .map(|o| format!("{o} → {}", file.path))
                        .unwrap_or_else(|| file.path.clone());
                    if ui
                        .selectable_label(selected, RichText::new(display))
                        .on_hover_text(&file.path)
                        .clicked()
                    {
                        select_file(app, &file.path, file.staged && !file.unstaged);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("⨯")
                            .on_hover_text("Discard changes (irreversible)")
                            .clicked()
                        {
                            discard_file(app, &file.path);
                        }
                    });
                });
            }
        },
    );

    ui.separator();
    commit_box(app, ui);
}

fn select_file(app: &mut App, path: &str, staged: bool) {
    app.selected_file = Some(path.to_string());
    app.selected_commit = None;
    let Some(repo) = app.repo.clone() else { return };
    let path = path.to_string();
    app.worker.spawn(move || {
        let text = repo
            .diff_file(&path, staged)
            .unwrap_or_else(|e| format!("(cannot diff: {e})"));
        let text =
            if text.trim().is_empty() { "(no textual diff, possibly binary)".into() } else { text };
        Msg::Diff { title: path, text }
    });
}

fn discard_file(app: &mut App, path: &str) {
    let Some(repo) = app.repo.clone() else { return };
    let path = path.to_string();
    app.worker.spawn(move || Msg::Done {
        message: strerr(
            repo.discard(std::slice::from_ref(&path))
                .map(|_| format!("Discarded changes to {path}")),
        ),
        refresh: true,
    });
}

fn commit_box(app: &mut App, ui: &mut egui::Ui) {
    ui.label(RichText::new("COMMIT").color(theme::EMBER).small());
    ui.add(
        egui::TextEdit::singleline(&mut app.commit_summary)
            .hint_text("Summary (required)")
            .desired_width(f32::INFINITY),
    );
    ui.add(
        egui::TextEdit::multiline(&mut app.commit_description)
            .hint_text("Description")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );

    ui.horizontal(|ui| {
        let ai_enabled = !app.ai_busy && app.repo.is_some();
        let ai_text = if app.ai_busy { "⏳ Generating…" } else { "✨ AI message" };
        if ui
            .add_enabled(ai_enabled, egui::Button::new(ai_text).fill(theme::TEAL.linear_multiply(0.25)))
            .on_hover_text("Generate commit message with Ollama")
            .clicked()
        {
            app.generate_ai_message();
        }
        let model = app.config.ollama_model.clone().unwrap_or_else(|| "no model".into());
        egui::ComboBox::from_id_salt("ai-model")
            .selected_text(model)
            .show_ui(ui, |ui| {
                let names: Vec<String> =
                    app.ollama_models.iter().map(|m| m.name.clone()).collect();
                for name in names {
                    let is_selected = app.config.ollama_model.as_deref() == Some(name.as_str());
                    if ui.selectable_label(is_selected, &name).clicked() {
                        app.config.ollama_model = Some(name.clone());
                        app.config.save();
                    }
                }
            });
    });

    let branch = app.status.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
    let can_commit =
        !app.commit_summary.trim().is_empty() && !app.files_for_commit().is_empty();
    let commit_btn = egui::Button::new(
        RichText::new(format!("Commit to {branch}")).strong().color(Color32::BLACK),
    )
    .fill(theme::EMBER)
    .min_size(egui::vec2(ui.available_width(), 32.0));
    if ui.add_enabled(can_commit, commit_btn).clicked() {
        app.do_commit();
    }
}

fn history_tab(app: &mut App, ui: &mut egui::Ui) {
    let commits = app.log.clone();
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if commits.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No commits yet").color(theme::FG_DIM));
            });
        }
        for commit in &commits {
            let selected = app.selected_commit.as_deref() == Some(&commit.sha);
            let heading = RichText::new(&commit.subject).strong();
            let meta = RichText::new(format!(
                "● {} · {} · {}",
                commit.short_sha,
                commit.author,
                commit.date.get(..10).unwrap_or(&commit.date)
            ))
            .color(theme::FG_DIM)
            .small();
            let response = ui.selectable_label(selected, heading);
            ui.label(meta);
            ui.separator();
            if response.clicked() {
                app.selected_commit = Some(commit.sha.clone());
                app.selected_file = None;
                let Some(repo) = app.repo.clone() else { return };
                let sha = commit.sha.clone();
                let title = format!("{} {}", commit.short_sha, commit.subject);
                app.worker.spawn(move || {
                    let text = repo
                        .diff_commit(&sha)
                        .unwrap_or_else(|e| format!("(cannot show commit: {e})"));
                    Msg::Diff { title, text }
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Diff panel
// ---------------------------------------------------------------------------

/// Central panel rendering the current diff with syntax-ish coloring.
pub fn diff_panel(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(0.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    let title = if app.diff_title.is_empty() {
                        "Select a file to view its diff"
                    } else {
                        &app.diff_title
                    };
                    ui.label(RichText::new(title).color(theme::EMBER).strong());
                });
            ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(4.0);
                for line in app.diff_text.lines() {
                    let (color, bg) = diff_line_style(line);
                    let text = RichText::new(line).monospace().color(color);
                    match bg {
                        Some(bg) => {
                            egui::Frame::new().fill(bg).show(ui, |ui| {
                                ui.label(text);
                            });
                        }
                        None => {
                            ui.label(text);
                        }
                    }
                }
            });
        });
}

fn diff_line_style(line: &str) -> (Color32, Option<Color32>) {
    if line.starts_with("+++") || line.starts_with("---") {
        (theme::FG_DIM, None)
    } else if line.starts_with('+') {
        (theme::ADD, Some(theme::ADD.linear_multiply(0.08)))
    } else if line.starts_with('-') {
        (theme::DEL, Some(theme::DEL.linear_multiply(0.08)))
    } else if line.starts_with("@@") {
        (theme::TEAL, Some(theme::TEAL.linear_multiply(0.08)))
    } else if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("commit ")
        || line.starts_with("Author")
        || line.starts_with("Date")
    {
        (theme::FG_DIM, None)
    } else {
        (theme::FG, None)
    }
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

/// Bottom-center transient notifications.
pub fn toasts(app: &mut App, ctx: &egui::Context) {
    let Some(toast) = &app.toast else { return };
    if std::time::Instant::now() > toast.until {
        app.toast = None;
        return;
    }
    let (border, color) =
        if toast.error { (theme::DANGER, theme::DANGER) } else { (theme::TEAL, theme::FG) };
    egui::Area::new("toast".into())
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PANEL)
                .stroke(egui::Stroke::new(1.0_f32, border))
                .corner_radius(999.0)
                .inner_margin(egui::Margin::symmetric(18, 10))
                .show(ui, |ui| {
                    ui.label(RichText::new(&toast.text).color(color));
                });
        });
}
