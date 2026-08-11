//! Main panels: toolbar, sidebar (changes/history), diff view, toasts.

use super::theme;
use super::worker::{pickable_branches, strerr, Msg};
use super::{App, Dialog, Tab};
use crate::git::{FileStatus, RepoState};
use egui::text::LayoutJob;
use egui::{Color32, FontId, RichText, ScrollArea, TextFormat};

// ---------------------------------------------------------------------------
// Toolbar (GitHub Desktop style: three large segments)
// ---------------------------------------------------------------------------

/// Builds the two-line text used by toolbar segments: a small dim caption
/// above a bold value, like GitHub Desktop's header buttons.
fn segment_text(caption: &str, value: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        caption,
        0.0,
        TextFormat { font_id: FontId::proportional(10.0), color: theme::FG_DIM, ..Default::default() },
    );
    job.append(
        &format!("\n{value}"),
        0.0,
        TextFormat { font_id: FontId::proportional(14.0), color: theme::FG, ..Default::default() },
    );
    job
}

fn segment(ui: &mut egui::Ui, caption: &str, value: &str, min_width: f32) -> egui::Response {
    let button = egui::Button::new(segment_text(caption, value))
        .min_size(egui::vec2(min_width, 46.0))
        .fill(theme::PANEL);
    ui.add(button)
}

/// Top toolbar: repository, branch, and one context-aware sync action,
/// plus pull request / GitHub / settings on the right.
pub fn toolbar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("toolbar")
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(8.0))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.horizontal(|ui| {
                // 1. Current repository (dropdown of recent repos)
                repo_menu(app, ui);

                // 2. Current branch (menu)
                branch_menu(app, ui);
                checks_badge(app, ui);

                // 3. Context-aware sync action
                sync_segment(app, ui);

                // Right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").on_hover_text("Settings").clicked() {
                        app.dialog = Dialog::Settings;
                    }
                    let gh_label = app
                        .gh
                        .user
                        .as_ref()
                        .map(|u| u.login.clone())
                        .unwrap_or_else(|| "Sign in".into());
                    if ui.button(gh_label.clone()).on_hover_text("GitHub").clicked() {
                        app.dialog = Dialog::GitHub;
                    }
                    if ui.button("Pull Request").clicked() {
                        open_pr_dialog(app);
                    }
                });
            });
            state_banner(app, ui);
        });
}

/// Repository dropdown: recent repositories saved in the local config, with
/// repair options for missing paths and an "Add repository" entry.
fn repo_menu(app: &mut App, ui: &mut egui::Ui) {
    let repo_name = app
        .repo
        .as_ref()
        .map(|r| r.name())
        .unwrap_or_else(|| "Choose…".into());

    ui.menu_button(segment_text("CURRENT REPOSITORY", &repo_name), |ui| {
        ui.set_min_width(320.0);
        ui.label(RichText::new("RECENT REPOSITORIES").color(theme::EMBER).small());

        let current_path = app.repo.as_ref().map(|r| r.path().display().to_string());
        let recents = app.config.recent_repos.clone();
        let mut remove: Option<String> = None;

        for path in &recents {
            let exists = std::path::Path::new(path).exists();
            let name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            let is_current = current_path.as_deref() == Some(path.as_str());

            ui.horizontal(|ui| {
                if exists {
                    let marker = if is_current { "» " } else { "    " };
                    if ui
                        .button(format!("{marker}{name}"))
                        .on_hover_text(path)
                        .clicked()
                    {
                        app.open_repo(path);
                        ui.close_menu();
                    }
                } else {
                    // Missing on disk: offer repair or removal.
                    ui.label(
                        RichText::new(format!("    {name} (missing)")).color(theme::FG_DIM),
                    )
                    .on_hover_text(path);
                    if ui.small_button("Change path…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new()
                            .set_title("Locate the repository folder")
                            .pick_folder()
                        {
                            remove = Some(path.clone());
                            app.open_repo(&folder.display().to_string());
                        }
                        ui.close_menu();
                    }
                    if ui.small_button("Remove").clicked() {
                        remove = Some(path.clone());
                    }
                }
            });
        }
        if recents.is_empty() {
            ui.label(RichText::new("No recent repositories").color(theme::FG_DIM));
        }

        if let Some(path) = remove {
            app.config.recent_repos.retain(|p| p != &path);
            app.config.save();
        }

        ui.separator();
        if ui.button("Add repository…").clicked() {
            app.dialog = Dialog::RepoPicker;
            ui.close_menu();
        }
    })
    .response
    .on_hover_text("Switch repository");
}

/// Small CI status badge for the current branch (GitHub Actions check runs).
fn checks_badge(app: &mut App, ui: &mut egui::Ui) {
    use crate::github::CheckState;
    let Some(summary) = app.branch_checks.clone() else { return };
    if summary.state == CheckState::None {
        return;
    }
    let (symbol, color) = match summary.state {
        CheckState::Passing => ("OK", theme::ADD),
        CheckState::Failing => ("FAIL", theme::DANGER),
        CheckState::Pending => ("RUNNING", theme::WARN),
        CheckState::None => unreachable!(),
    };
    let text = format!("CI {symbol} ({}/{})", summary.passed, summary.total);
    let badge = egui::Button::new(RichText::new(text).color(color).small())
        .fill(theme::PANEL)
        .min_size(egui::vec2(0.0, 46.0));
    let hover = format!(
        "GitHub Actions: {} passed, {} failed, {} running",
        summary.passed, summary.failed, summary.pending
    );
    if ui.add(badge).on_hover_text(hover).clicked() {
        // Open the branch's checks page on GitHub.
        if let (Some(repo), Some(status)) = (app.repo.as_ref(), app.status.as_ref()) {
            if let Some(slug) = origin_slug(repo) {
                let _ = open::that(format!(
                    "https://github.com/{}/{}/actions?query=branch%3A{}",
                    slug.owner, slug.repo, status.branch
                ));
            }
        }
    }
}

fn branch_menu(app: &mut App, ui: &mut egui::Ui) {
    let current = app
        .status
        .as_ref()
        .map(|s| s.branch.clone())
        .unwrap_or_else(|| "—".into());

    let response = ui.menu_button(segment_text("CURRENT BRANCH", &current), |ui| {
        ui.set_min_width(320.0);

        // New branch
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.new_branch_name)
                    .hint_text("New branch name")
                    .desired_width(190.0),
            );
            if ui.button("Create").clicked() && !app.new_branch_name.trim().is_empty() {
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

        // Filter + branch lists
        ui.add(
            egui::TextEdit::singleline(&mut app.branch_filter)
                .hint_text("Filter branches…")
                .desired_width(f32::INFINITY),
        );
        let filter = app.branch_filter.to_lowercase();
        let (locals, remotes) = app
            .branches
            .as_ref()
            .map(|b| (b.local.clone(), b.remote.clone()))
            .unwrap_or_default();

        ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            ui.label(RichText::new("LOCAL").color(theme::EMBER).small());
            for branch in locals.iter().filter(|b| b.name.to_lowercase().contains(&filter)) {
                let marker = if branch.current { "» " } else { "    " };
                if ui.button(format!("{marker}{}", branch.name)).clicked() {
                    checkout(app, &branch.name);
                    ui.close_menu();
                }
            }
            if !remotes.is_empty() {
                ui.label(RichText::new("REMOTE").color(theme::EMBER).small());
                for branch in remotes.iter().filter(|b| b.name.to_lowercase().contains(&filter))
                {
                    if ui.button(format!("    {}", branch.name)).clicked() {
                        let local =
                            branch.name.split_once('/').map(|(_, l)| l).unwrap_or(&branch.name);
                        checkout(app, local);
                        ui.close_menu();
                    }
                }
            }
        });

        // Branch actions, like GitHub Desktop's Branch menu
        ui.separator();
        let others = app.branches.as_ref().map(pickable_branches).unwrap_or_default();
        ui.menu_button(format!("Merge into {current}…"), |ui| {
            ui.set_min_width(240.0);
            for branch in &others {
                if ui.button(&branch.name).clicked() {
                    if let Some(repo) = app.repo.clone() {
                        let name = branch.name.clone();
                        app.busy = true;
                        app.worker.spawn(move || Msg::MergeOutcome(repo.merge(&name)));
                    }
                    ui.close_menu();
                }
            }
        });
        ui.menu_button(format!("Rebase {current} onto…"), |ui| {
            ui.set_min_width(240.0);
            for branch in &others {
                if ui.button(&branch.name).clicked() {
                    if let Some(repo) = app.repo.clone() {
                        let name = branch.name.clone();
                        app.busy = true;
                        app.worker.spawn(move || Msg::MergeOutcome(repo.rebase(&name)));
                    }
                    ui.close_menu();
                }
            }
        });
        if ui.button("Create Pull Request…").clicked() {
            open_pr_dialog(app);
            ui.close_menu();
        }

        // Stash
        ui.separator();
        if ui.button("Stash all changes").clicked() {
            if let Some(repo) = app.repo.clone() {
                app.worker.spawn(move || Msg::Done {
                    message: strerr(repo.stash_save("").map(|_| "Changes stashed.".to_string())),
                    refresh: true,
                });
            }
            ui.close_menu();
        }
        let stashes = app.stashes.clone();
        ui.menu_button(format!("Stashes ({})", stashes.len()), |ui| {
            ui.set_min_width(260.0);
            if stashes.is_empty() {
                ui.label(RichText::new("No stashes").color(theme::FG_DIM));
            }
            for stash in &stashes {
                ui.horizontal(|ui| {
                    ui.label(truncate(&stash.message, 26)).on_hover_text(&stash.message);
                    if ui.small_button("Apply").clicked() {
                        if let Some(repo) = app.repo.clone() {
                            let idx = stash.index;
                            app.worker.spawn(move || Msg::Done {
                                message: strerr(
                                    repo.stash_pop(idx).map(|_| "Stash applied.".to_string()),
                                ),
                                refresh: true,
                            });
                        }
                        ui.close_menu();
                    }
                    if ui.small_button("Drop").clicked() {
                        if let Some(repo) = app.repo.clone() {
                            let idx = stash.index;
                            app.worker.spawn(move || Msg::Done {
                                message: strerr(
                                    repo.stash_drop(idx).map(|_| "Stash dropped.".to_string()),
                                ),
                                refresh: true,
                            });
                        }
                        ui.close_menu();
                    }
                });
            }
        });

        // Undo
        if ui
            .button("Undo last commit")
            .on_hover_text("Soft reset: keeps the changes staged")
            .clicked()
        {
            if let Some(repo) = app.repo.clone() {
                app.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.undo_last_commit()
                            .map(|_| "Last commit undone (changes kept).".to_string()),
                    ),
                    refresh: true,
                });
            }
            ui.close_menu();
        }

        // Tags
        ui.menu_button(format!("Tags ({})", app.tags.len()), |ui| {
            ui.set_min_width(240.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.tag_name_input)
                        .hint_text("v1.0.0")
                        .desired_width(120.0),
                );
                if ui.button("Tag HEAD").clicked() && !app.tag_name_input.trim().is_empty() {
                    if let Some(repo) = app.repo.clone() {
                        let name = app.tag_name_input.trim().to_string();
                        app.tag_name_input.clear();
                        app.worker.spawn(move || Msg::Done {
                            message: strerr(
                                repo.create_tag(&name, "").map(|_| format!("Tagged {name}")),
                            ),
                            refresh: true,
                        });
                    }
                    ui.close_menu();
                }
            });
            let tags = app.tags.clone();
            for tag in tags.iter().take(20) {
                ui.horizontal(|ui| {
                    ui.label(tag);
                    if ui.small_button("Push").clicked() {
                        if let Some(repo) = app.repo.clone() {
                            let name = tag.clone();
                            let token = app.gh_token();
                            app.worker.spawn(move || Msg::Done {
                                message: strerr(
                                    repo.push_tag(&name, token.as_deref())
                                        .map(|_| format!("Pushed tag {name}")),
                                ),
                                refresh: false,
                            });
                        }
                        ui.close_menu();
                    }
                });
            }
        });

        // Branch management
        ui.separator();
        let manageable: Vec<String> = app
            .branches
            .as_ref()
            .map(|b| b.local.iter().filter(|br| !br.current).map(|br| br.name.clone()).collect())
            .unwrap_or_default();
        ui.menu_button("Delete branch…", |ui| {
            ui.set_min_width(200.0);
            for name in &manageable {
                if ui.button(name).clicked() {
                    if let Some(repo) = app.repo.clone() {
                        let name = name.clone();
                        app.worker.spawn(move || Msg::Done {
                            message: strerr(
                                repo.delete_branch(&name, false)
                                    .map(|_| format!("Deleted {name}")),
                            ),
                            refresh: true,
                        });
                    }
                    ui.close_menu();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.rename_branch_input)
                    .hint_text("Rename current to…")
                    .desired_width(150.0),
            );
            if ui.button("Rename").clicked() && !app.rename_branch_input.trim().is_empty() {
                if let Some(repo) = app.repo.clone() {
                    let old = current.clone();
                    let new = app.rename_branch_input.trim().to_string();
                    app.rename_branch_input.clear();
                    app.worker.spawn(move || Msg::Done {
                        message: strerr(
                            repo.rename_branch(&old, &new).map(|_| format!("Renamed to {new}")),
                        ),
                        refresh: true,
                    });
                }
                ui.close_menu();
            }
        });
    });
    response.response.on_hover_text("Switch branches or start branch actions");
}

fn checkout(app: &mut App, name: &str) {
    let Some(repo) = app.repo.clone() else { return };
    let name = name.to_string();
    app.worker.spawn(move || Msg::Done {
        message: strerr(repo.checkout(&name).map(|_| format!("Switched to {name}"))),
        refresh: true,
    });
}

/// One context-aware sync segment, like GitHub Desktop's third header button:
/// Publish when there is no upstream, Pull when behind, Push when ahead,
/// otherwise Fetch. Publishing without any remote asks for a remote URL.
fn sync_segment(app: &mut App, ui: &mut egui::Ui) {
    let (ahead, behind, has_upstream, has_remote) = app
        .status
        .as_ref()
        .map(|s| (s.ahead, s.behind, s.has_upstream, s.has_remote))
        .unwrap_or((0, 0, false, false));

    let commits = |n: u32| if n == 1 { "1 commit".to_string() } else { format!("{n} commits") };

    let (caption, value, action) = if app.repo.is_none() {
        ("REMOTE", "Fetch origin".to_string(), "fetch")
    } else if !has_remote {
        // No remote at all: publishing first needs a URL.
        if ahead > 0 {
            ("PUBLISH", format!("Publish {}", commits(ahead)), "add-remote")
        } else {
            ("PUBLISH", "Publish branch".to_string(), "add-remote")
        }
    } else if !has_upstream {
        if ahead > 0 {
            ("PUBLISH", format!("Publish {}", commits(ahead)), "push")
        } else {
            ("PUBLISH", "Publish branch".to_string(), "push")
        }
    } else if behind > 0 {
        ("PULL", format!("Pull {}", commits(behind)), "pull")
    } else if ahead > 0 {
        ("PUSH", format!("Push {}", commits(ahead)), "push")
    } else {
        ("REMOTE", "Fetch origin".to_string(), "fetch")
    };

    let response = segment(ui, caption, &value, 170.0)
        .on_hover_text("Right-click for all sync actions");
    response.context_menu(|ui| {
        if ui.button("Fetch").clicked() {
            run_sync(app, "fetch");
            ui.close_menu();
        }
        if ui.button("Pull").clicked() {
            run_sync(app, "pull");
            ui.close_menu();
        }
        if ui.button("Push").clicked() {
            run_sync(app, "push");
            ui.close_menu();
        }
    });
    if response.clicked() {
        if action == "add-remote" {
            app.remote_url_input.clear();
            app.dialog = Dialog::AddRemote;
        } else {
            run_sync(app, action);
        }
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
                            .small_button("x")
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
    app.show_staged = staged;
    app.blame = None;
    app.hunks.clear();
    app.commit_file_list.clear();
    load_file_diff(app);
}

/// Loads the diff (and hunks for the unstaged side) of the selected file.
pub fn load_file_diff(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let Some(path) = app.selected_file.clone() else { return };
    let staged = app.show_staged;
    app.worker.spawn(move || {
        let text = repo
            .diff_file(&path, staged)
            .unwrap_or_else(|e| format!("(cannot diff: {e})"));
        let text =
            if text.trim().is_empty() { "(no textual diff, possibly binary)".into() } else { text };
        Msg::Diff { title: path, text }
    });
    if !staged {
        let repo = app.repo.clone().unwrap();
        let path = app.selected_file.clone().unwrap();
        app.worker.spawn(move || {
            let hunks = repo.hunks(&path).unwrap_or_default();
            Msg::Hunks { file: path, hunks }
        });
    }
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
        let ai_text = if app.ai_busy { "Generating…" } else { "AI message" };
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
        ui.checkbox(&mut app.amend, "Amend")
            .on_hover_text("Rewrite the last commit instead of creating a new one");
    });

    let branch = app.status.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
    let can_commit = !app.commit_summary.trim().is_empty()
        && (!app.files_for_commit().is_empty() || app.amend);
    let label = if app.amend {
        format!("Amend last commit on {branch}")
    } else {
        format!("Commit to {branch}")
    };
    let commit_btn = egui::Button::new(RichText::new(label).strong().color(Color32::BLACK))
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
                "{} · {} · {}",
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
                app.commit_file_list.clear();
                let Some(repo) = app.repo.clone() else { return };
                let sha = commit.sha.clone();
                let title = format!("{} {}", commit.short_sha, commit.subject);
                {
                    let repo = repo.clone();
                    let sha = sha.clone();
                    app.worker.spawn(move || {
                        let text = repo
                            .diff_commit(&sha)
                            .unwrap_or_else(|e| format!("(cannot show commit: {e})"));
                        Msg::Diff { title, text }
                    });
                }
                app.worker.spawn(move || {
                    let files = repo.commit_files(&sha).unwrap_or_default();
                    Msg::CommitFiles { sha, files }
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Diff panel
// ---------------------------------------------------------------------------

/// Central panel rendering the current diff with syntax-ish coloring,
/// plus per-hunk staging, staged/unstaged toggle, and blame view.
pub fn diff_panel(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(0.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let title = if app.diff_title.is_empty() {
                            "Select a file to view its diff"
                        } else {
                            &app.diff_title
                        };
                        ui.label(RichText::new(title).color(theme::EMBER).strong());
                        // File-level controls only when a working file is selected.
                        if app.selected_file.is_some() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let staged_label =
                                        if app.show_staged { "Staged diff" } else { "Unstaged diff" };
                                    if ui
                                        .selectable_label(app.show_staged, staged_label)
                                        .on_hover_text("Toggle staged/unstaged view")
                                        .clicked()
                                    {
                                        app.show_staged = !app.show_staged;
                                        app.blame = None;
                                        load_file_diff(app);
                                    }
                                    let blame_on = app.blame.is_some();
                                    if ui
                                        .selectable_label(blame_on, "Blame")
                                        .on_hover_text("Show line-by-line authorship")
                                        .clicked()
                                    {
                                        if blame_on {
                                            app.blame = None;
                                        } else {
                                            load_blame(app);
                                        }
                                    }
                                    if ui
                                        .button("Ignore")
                                        .on_hover_text("Add this file to .gitignore")
                                        .clicked()
                                    {
                                        ignore_selected(app);
                                    }
                                },
                            );
                        }
                    });
                });

            // History mode: show the commit's file list above the patch.
            if app.selected_commit.is_some() && !app.commit_file_list.is_empty() {
                commit_file_strip(app, ui);
            }

            if let Some(blame) = app.blame.clone() {
                blame_view(ui, &blame);
                return;
            }

            // Hunk staging bar for the unstaged view.
            if app.selected_file.is_some() && !app.show_staged && !app.hunks.is_empty() {
                hunk_bar(app, ui);
            }

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

/// Buttons to stage individual hunks of the selected file.
fn hunk_bar(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::PANEL)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} hunk(s):", app.hunks.len()))
                        .color(theme::FG_DIM)
                        .small(),
                );
                let hunks = app.hunks.clone();
                for (i, hunk) in hunks.iter().enumerate() {
                    if ui
                        .small_button(format!("Stage hunk {}", i + 1))
                        .on_hover_text(&hunk.header)
                        .clicked()
                    {
                        if let Some(repo) = app.repo.clone() {
                            match repo.stage_hunk(hunk) {
                                Ok(()) => {
                                    app.toast(format!("Staged hunk {}", i + 1), false);
                                    load_file_diff(app);
                                    app.refresh();
                                }
                                Err(e) => app.toast(e.to_string(), true),
                            }
                        }
                    }
                }
            });
        });
}

/// Horizontal strip listing files changed in the selected commit.
fn commit_file_strip(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::PANEL)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} file(s):", app.commit_file_list.len()))
                        .color(theme::FG_DIM)
                        .small(),
                );
                let files = app.commit_file_list.clone();
                let sha = app.selected_commit.clone().unwrap_or_default();
                for f in &files {
                    let (glyph, color) = status_glyph(Some(f.status), false);
                    let label = RichText::new(format!("{glyph} {}", f.path)).color(color).small();
                    if ui.button(label).on_hover_text("Show only this file's changes").clicked() {
                        if let Some(repo) = app.repo.clone() {
                            let sha = sha.clone();
                            let path = f.path.clone();
                            let title = format!("{} — {}", &sha[..7.min(sha.len())], path);
                            app.worker.spawn(move || {
                                let text = repo
                                    .diff_commit_file(&sha, &path)
                                    .unwrap_or_else(|e| format!("(cannot diff: {e})"));
                                Msg::Diff { title, text }
                            });
                        }
                    }
                }
            });
        });
}

/// Renders blame output with sha/author gutters.
fn blame_view(ui: &mut egui::Ui, blame: &[crate::git::BlameLine]) {
    ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);
        for b in blame {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&b.sha).monospace().color(theme::TEAL).small());
                ui.label(
                    RichText::new(format!("{:<12}", truncate(&b.author, 12)))
                        .monospace()
                        .color(theme::FG_DIM)
                        .small(),
                );
                ui.label(RichText::new(&b.line).monospace());
            });
        }
    });
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

fn load_blame(app: &mut App) {
    let (Some(repo), Some(path)) = (app.repo.clone(), app.selected_file.clone()) else { return };
    match repo.blame(&path) {
        Ok(blame) => app.blame = Some(blame),
        Err(e) => app.toast(e.to_string(), true),
    }
}

fn ignore_selected(app: &mut App) {
    let (Some(repo), Some(path)) = (app.repo.clone(), app.selected_file.clone()) else { return };
    match repo.ignore(&path) {
        Ok(()) => {
            app.toast(format!("Added {path} to .gitignore"), false);
            app.selected_file = None;
            app.diff_text.clear();
            app.diff_title.clear();
            app.refresh();
        }
        Err(e) => app.toast(e.to_string(), true),
    }
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
