//! Main panels: toolbar, sidebar (changes/history), diff view, toasts.

use super::theme;
use super::worker::{pickable_branches, strerr, Msg};
use super::{App, Dialog, Tab};
use crate::git::{FileStatus, PullStrategy, RepoState};
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
        &caption.to_uppercase(),
        0.0,
        TextFormat { font_id: FontId::proportional(9.5), color: theme::fg_dim(), ..Default::default() },
    );
    job.append(
        &format!("\n{value}"),
        0.0,
        TextFormat { font_id: FontId::proportional(14.5), color: theme::fg(), ..Default::default() },
    );
    job
}

/// Uniform width for all toolbar segments.
const SEGMENT_W: f32 = 190.0;

/// How many "Stage hunk N" buttons the hunk bar shows before collapsing the
/// rest behind a "Show N more" toggle.
pub(crate) const HUNK_BAR_LIMIT: usize = 10;

/// Menu-style toolbar segment: same size as `segment`, opens a dropdown.
fn segment_menu<R>(
    ui: &mut egui::Ui,
    caption: &str,
    value: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    use egui::containers::menu::{MenuButton, MenuConfig};
    ui.scope(|ui| {
        ui.spacing_mut().interact_size = egui::vec2(SEGMENT_W, theme::SEGMENT_H);
        ui.spacing_mut().button_padding = egui::vec2(12.0, 6.0);
        // CloseOnClickOutside keeps the menu open when interacting with
        // text inputs inside it (filter/search boxes, name fields).
        // Item buttons still close explicitly via ui.close(). Submenus
        // inherit this behavior through the menu config tag.
        let (response, inner) = MenuButton::new(segment_text(caption, value))
            .config(
                MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, add_contents);
        egui::InnerResponse::new(inner.map(|ir| ir.inner), response)
    })
    .inner
}

fn segment(ui: &mut egui::Ui, caption: &str, value: &str, min_width: f32) -> egui::Response {
    let button = egui::Button::new(segment_text(caption, value))
        .min_size(egui::vec2(min_width, theme::SEGMENT_H))
        .fill(theme::panel())
        .stroke(egui::Stroke::new(1.0_f32, theme::border()))
        .corner_radius(theme::RADIUS_MD as f32);
    ui.add(button)
}

/// Top toolbar: repository, branch, and one context-aware sync action,
/// plus pull request / GitHub / settings on the right.
pub fn toolbar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("toolbar")
        .frame(egui::Frame::new().fill(theme::bg()).inner_margin(8.0))
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

                // Graph toggle
                let graph_btn = if app.graph_open { "Graph ✦" } else { "Graph" };
                if ui
                    .selectable_label(app.graph_open, graph_btn)
                    .on_hover_text("Animated commit graph of all branches")
                    .clicked()
                {
                    app.graph_open = !app.graph_open;
                    if app.graph_open {
                        app.load_graph();
                    }
                }

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
                    if ui
                        .button("Stack")
                        .on_hover_text(
                            "Stacked pull requests: a chain of branches, each \
                             reviewed against the one below it",
                        )
                        .clicked()
                    {
                        app.open_stack();
                    }
                });
            });
            state_banner(app, ui);
            stash_banner(app, ui);
        });
}

/// Subtle banner shown while stashes exist, so stashed work is never
/// forgotten (and later reapplied onto conflicting changes by surprise).
fn stash_banner(app: &mut App, ui: &mut egui::Ui) {
    let count = app.stashes.len();
    if count == 0 {
        return;
    }
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::teal().linear_multiply(0.10))
        .stroke(egui::Stroke::new(1.0_f32, theme::teal().linear_multiply(0.5)))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let newest = app
                    .stashes
                    .first()
                    .map(|s| truncate(&s.message, 40))
                    .unwrap_or_default();
                let text = if count == 1 {
                    format!("1 stash: {newest}")
                } else {
                    format!("{count} stashes, newest: {newest}")
                };
                ui.label(RichText::new(text).color(theme::teal()).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Apply newest")
                        .on_hover_text(
                            "Restores the most recent stash into the working tree.                              Conflicts open the resolver.",
                        )
                        .clicked()
                    {
                        if let Some(repo) = app.repo.clone() {
                            app.worker.spawn(move || Msg::Done {
                                message: strerr(
                                    repo.stash_pop(0)
                                        .map(|_| "Stash applied.".to_string()),
                                ),
                                refresh: true,
                            });
                        }
                    }
                    ui.label(
                        RichText::new("more in the branch menu · ")
                            .color(theme::fg_dim())
                            .small(),
                    );
                });
            });
        });
}

/// Dim, italic hint text for input fields, clearly distinct from content.
pub fn dim_hint(text: &str) -> RichText {
    RichText::new(text).color(theme::fg_dim().linear_multiply(0.5)).italics()
}

/// Repository dropdown: recent repositories saved in the local config, with
/// repair options for missing paths and an "Add repository" entry.
fn repo_menu(app: &mut App, ui: &mut egui::Ui) {
    let repo_name = app
        .repo
        .as_ref()
        .map(|r| r.name())
        .unwrap_or_else(|| "Choose…".into());

    segment_menu(ui, "CURRENT REPOSITORY", &repo_name, |ui| {
        ui.set_min_width(320.0);
        ui.label(theme::overline("RECENT REPOSITORIES"));

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
                        ui.close();
                    }
                } else {
                    // Missing on disk: offer repair or removal.
                    ui.label(
                        RichText::new(format!("    {name} (missing)")).color(theme::fg_dim()),
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
                        ui.close();
                    }
                    if ui.small_button("Remove").clicked() {
                        remove = Some(path.clone());
                    }
                }
            });
        }
        if recents.is_empty() {
            ui.label(RichText::new("No recent repositories").color(theme::fg_dim()));
        }

        if let Some(path) = remove {
            app.config.recent_repos.retain(|p| p != &path);
            app.config.save();
        }

        ui.separator();
        if ui.button("Add repository…").clicked() {
            app.dialog = Dialog::RepoPicker;
            ui.close();
        }
    })
    .response
    .on_hover_text("Switch repository");
}

/// CI status badge for the current branch, with a dropdown listing every
/// check run (failures first). Clicking a run opens its page on GitHub.
fn checks_badge(app: &mut App, ui: &mut egui::Ui) {
    use crate::github::CheckState;
    let Some(summary) = app.branch_checks.clone() else { return };
    if summary.state == CheckState::None {
        return;
    }
    let (symbol, color) = match summary.state {
        CheckState::Passing => ("OK", theme::add()),
        CheckState::Failing => ("FAIL", theme::danger()),
        CheckState::Pending => ("RUNNING", theme::warn()),
        CheckState::None => unreachable!(),
    };
    // Compose "OK (4/4) · main OK" with the main part colored.
    let main_part = app.main_checks.as_ref().map(|(name, s)| {
        let sym = match s.state {
            CheckState::Passing => "OK",
            CheckState::Failing => "FAIL",
            CheckState::Pending => "RUN",
            CheckState::None => "-",
        };
        (name.clone(), sym, s.state)
    });
    let text = match &main_part {
        Some((name, sym, _)) => format!(
            "{symbol} ({}/{}) · {name} {sym}",
            summary.passed, summary.total
        ),
        None => format!("{symbol} ({}/{})", summary.passed, summary.total),
    };
    let _ = color; // per-run colors shown inside the dropdown

    segment_menu(ui, "CI STATUS", &text, |ui| {
        // Default-branch summary row, green when healthy.
        if let Some((name, _, state)) = &main_part {
            let (label, mcolor) = match state {
                CheckState::Passing => (format!("{name}: all checks passing"), theme::add()),
                CheckState::Failing => (format!("{name}: checks failing"), theme::danger()),
                CheckState::Pending => (format!("{name}: checks running"), theme::warn()),
                CheckState::None => (format!("{name}: no checks"), theme::fg_dim()),
            };
            ui.label(RichText::new(label).color(mcolor).strong());
            ui.separator();
        }
        ui.set_min_width(340.0);
        ui.label(
            RichText::new(format!(
                "{} passed, {} failed, {} running",
                summary.passed, summary.failed, summary.pending
            ))
            .color(theme::fg_dim())
            .small(),
        );
        ui.separator();
        for run in &summary.runs {
            let (glyph, run_color) = match (run.status.as_str(), run.conclusion.as_str()) {
                ("completed", "success" | "neutral" | "skipped") => ("[pass]", theme::add()),
                ("completed", _) => ("[fail]", theme::danger()),
                _ => ("[running]", theme::warn()),
            };
            let detail = if run.status == "completed" {
                run.conclusion.clone()
            } else {
                run.status.replace('_', " ")
            };
            let row = RichText::new(format!("{glyph} {} ({detail})", run.name)).color(run_color);
            if ui
                .button(row)
                .on_hover_text("Open this check run on GitHub")
                .clicked()
            {
                if !run.html_url.is_empty() {
                    let _ = open::that(&run.html_url);
                }
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Open all checks on GitHub").clicked() {
            if let (Some(repo), Some(status)) = (app.repo.as_ref(), app.status.as_ref()) {
                if let Some(slug) = origin_slug(repo) {
                    let _ = open::that(format!(
                        "https://github.com/{}/{}/actions?query=branch%3A{}",
                        slug.owner, slug.repo, status.branch
                    ));
                }
            }
            ui.close();
        }
    });
}

fn branch_menu(app: &mut App, ui: &mut egui::Ui) {
    let current = app
        .status
        .as_ref()
        .map(|s| s.branch.clone())
        .unwrap_or_else(|| "—".into());

    let response = segment_menu(ui, "CURRENT BRANCH", &current, |ui| {
        ui.set_min_width(320.0);

        // New branch
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.new_branch_name)
                    .hint_text(dim_hint("New branch name"))
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
                ui.close();
            }
        });
        ui.separator();

        // Filter + branch lists
        ui.add(
            egui::TextEdit::singleline(&mut app.branch_filter)
                .hint_text(dim_hint("Filter branches…"))
                .desired_width(f32::INFINITY),
        );
        let filter = app.branch_filter.to_lowercase();
        let (locals, remotes) = app
            .branches
            .as_ref()
            .map(|b| (b.local.clone(), b.remote.clone()))
            .unwrap_or_default();

        // Local branches that track remotes; hide those remotes below.
        let local_names: std::collections::HashSet<String> =
            locals.iter().map(|b| b.name.clone()).collect();

        ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            // -- Local section --
            let local_matches: Vec<_> = locals
                .iter()
                .filter(|b| b.name.to_lowercase().contains(&filter))
                .collect();
            egui::Frame::new()
                .fill(theme::panel2())
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.label(
                        theme::overline(&format!("Local branches ({})", local_matches.len())),
                    );
                });
            if local_matches.is_empty() {
                ui.label(RichText::new("  none").color(theme::fg_dim()).small());
            }
            for branch in local_matches {
                let marker = if branch.current { "» " } else { "    " };
                if ui.button(format!("{marker}{}", branch.name)).clicked() {
                    checkout(app, &branch.name);
                    ui.close();
                }
            }

            // -- Remote section (only branches without a local counterpart) --
            let remote_matches: Vec<_> = remotes
                .iter()
                .filter(|b| b.name.to_lowercase().contains(&filter))
                .filter(|b| {
                    let short =
                        b.name.split_once('/').map(|(_, l)| l).unwrap_or(&b.name);
                    !local_names.contains(short)
                })
                .collect();
            ui.add_space(6.0);
            ui.separator();
            egui::Frame::new()
                .fill(theme::panel2())
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.label(
                        theme::overline(&format!("Remote branches ({})", remote_matches.len())).color(theme::teal()),
                    );
                });
            if remote_matches.is_empty() {
                ui.label(
                    RichText::new("  none (all remotes have local branches)")
                        .color(theme::fg_dim())
                        .small(),
                );
            }
            for branch in remote_matches {
                let label = RichText::new(format!("    {}", branch.name)).color(theme::teal());
                if ui
                    .button(label)
                    .on_hover_text("Creates a local tracking branch and switches to it")
                    .clicked()
                {
                    let local =
                        branch.name.split_once('/').map(|(_, l)| l).unwrap_or(&branch.name);
                    checkout(app, local);
                    ui.close();
                }
            }
        });

        // Branch actions, like GitHub Desktop's Branch menu
        ui.separator();
        let others = app.branches.as_ref().map(pickable_branches).unwrap_or_default();
        ui.menu_button(format!("Merge {current} into…"), |ui| {
            ui.set_min_width(260.0);
            ui.label(
                RichText::new(format!(
                    "Puts {current}'s commits onto the branch you pick"
                ))
                .color(theme::fg_dim())
                .small(),
            );
            let locals_only: Vec<_> =
                others.iter().filter(|b| !b.name.contains('/')).collect();
            for branch in locals_only {
                if ui.button(&branch.name).clicked() {
                    app.request_merge_into(&branch.name);
                    ui.close();
                }
            }
        });
        ui.menu_button(format!("Merge a branch into {current}…"), |ui| {
            ui.set_min_width(260.0);
            ui.label(
                RichText::new(format!("Brings the picked branch's commits into {current}"))
                    .color(theme::fg_dim())
                    .small(),
            );
            for branch in &others {
                if ui.button(&branch.name).clicked() {
                    if let Some(repo) = app.repo.clone() {
                        let name = branch.name.clone();
                        app.busy = true;
                        app.worker.spawn(move || Msg::MergeOutcome(repo.merge(&name)));
                    }
                    ui.close();
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
                    ui.close();
                }
            }
        });
        if ui.button("Create Pull Request…").clicked() {
            open_pr_dialog(app);
            ui.close();
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
            ui.close();
        }
        let stashes = app.stashes.clone();
        ui.menu_button(format!("Stashes ({})", stashes.len()), |ui| {
            ui.set_min_width(260.0);
            if stashes.is_empty() {
                ui.label(RichText::new("No stashes").color(theme::fg_dim()));
            }
            let current_branch =
                app.status.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
            for stash in &stashes {
                ui.horizontal(|ui| {
                    let here = stash.branch.as_deref() == Some(current_branch.as_str());
                    let label = if here {
                        RichText::new(truncate(&stash.message, 26))
                    } else {
                        RichText::new(format!(
                            "{} ({})",
                            truncate(&stash.message, 20),
                            stash.branch.as_deref().unwrap_or("?")
                        ))
                        .color(theme::fg_dim())
                    };
                    ui.label(label).on_hover_text(&stash.message);
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
                        ui.close();
                    }
                    if ui.small_button("Drop").clicked() {
                        app.confirm(crate::app::ConfirmAction::DropStash(stash.index));
                        ui.close();
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
            let subject = app.log.first().map(|c| c.subject.clone()).unwrap_or_default();
            app.confirm(crate::app::ConfirmAction::UndoCommit(subject));
            ui.close();
        }

        // Tags
        ui.menu_button(format!("Tags ({})", app.tags.len()), |ui| {
            ui.set_min_width(240.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.tag_name_input)
                        .hint_text(dim_hint("v1.0.0"))
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
                    ui.close();
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
                        ui.close();
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
                    app.confirm(crate::app::ConfirmAction::DeleteBranch(name.clone()));
                    ui.close();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.rename_branch_input)
                    .hint_text(dim_hint("Rename current to…"))
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
                ui.close();
            }
        });
    });
    response.response.on_hover_text("Switch branches or start branch actions");
}

fn checkout(app: &mut App, name: &str) {
    app.request_checkout(name);
}

/// One context-aware sync segment, like GitHub Desktop's third header button:
/// Publish when there is no upstream, Pull when behind, Push when ahead,
/// otherwise Fetch. Publishing without any remote asks for a remote URL.
/// Non-interactive toolbar segment with a circular spinner, shown while
/// a sync operation runs so the click visibly "took".
fn segment_spinner(ui: &mut egui::Ui, caption: &str, value: &str) {
    egui::Frame::new()
        .fill(theme::panel2())
        .stroke(egui::Stroke::new(1.0_f32, theme::teal()))
        .corner_radius(theme::RADIUS_MD as f32)
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(SEGMENT_W, theme::SEGMENT_H));
            ui.horizontal_centered(|ui| {
                ui.add_space(12.0);
                ui.add(egui::Spinner::new().size(16.0).color(theme::teal()));
                ui.add_space(6.0);
                ui.label(segment_text(caption, value));
            });
        });
}

fn sync_segment(app: &mut App, ui: &mut egui::Ui) {
    // A sync operation in flight: replace the button with a spinner so
    // the click is obviously being worked on (and cannot double-fire).
    if let Some(op) = app.sync_op {
        let (caption, value) = match op {
            "fetch" => ("REMOTE", "Fetching…"),
            "pull" | "pull-merge" | "pull-rebase" => ("PULL", "Pulling…"),
            "force-push" => ("PUSH", "Force-pushing…"),
            _ => ("PUSH", "Pushing…"),
        };
        segment_spinner(ui, caption, value);
        return;
    }

    let (ahead, behind, has_upstream, has_remote) = app
        .status
        .as_ref()
        .map(|s| (s.ahead, s.behind, s.has_upstream, s.has_remote))
        .unwrap_or((0, 0, false, false));

    let commits = |n: u32| if n == 1 { "1 commit".to_string() } else { format!("{n} commits") };

    // A CI-gated push in flight: reflect it on the button.
    if app.local_ci.pending_push.is_some() {
        segment_spinner(
            ui,
            "PUSH",
            &format!(
                "Checks {}/{}…",
                app.local_ci.finished(),
                app.local_ci.jobs.len()
            ),
        );
        return;
    }

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

    let response = segment(ui, caption, &value, SEGMENT_W)
        .on_hover_text("Right-click for all sync actions");
    response.context_menu(|ui| {
        if ui.button("Fetch").clicked() {
            run_sync(app, "fetch");
            ui.close();
        }
        if ui
            .button("Pull")
            .on_hover_text("Fast-forward only. Stops safely if your branch and origin have both moved.")
            .clicked()
        {
            run_sync(app, "pull");
            ui.close();
        }
        if ui
            .button("Pull (rebase)")
            .on_hover_text("Replays your local commits on top of origin. Keeps history linear.")
            .clicked()
        {
            run_sync(app, "pull-rebase");
            ui.close();
        }
        if ui
            .button("Pull (merge)")
            .on_hover_text("Joins the two histories with a merge commit.")
            .clicked()
        {
            run_sync(app, "pull-merge");
            ui.close();
        }
        if ui.button("Push").clicked() {
            run_sync(app, "push");
            ui.close();
        }
        if ui
            .button("Force push (with lease)")
            .on_hover_text("Needed after amend/rebase of pushed commits. Fails safely if the remote moved.")
            .clicked()
        {
            run_sync(app, "force-push");
            ui.close();
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
    let set_upstream = !app.status.as_ref().map(|s| s.has_upstream).unwrap_or(false);

    // Pushes go through the local-CI gate (on_push config).
    if action == "push" || action == "force-push" {
        app.push_with_ci(action, set_upstream);
        return;
    }

    let token = app.gh_token();
    app.busy = true;
    app.sync_op = Some(action);
    app.worker.spawn(move || {
        let auth = token.as_deref();
        let result = match action {
            "fetch" => repo.fetch(auth).map(|_| "Fetched.".to_string()),
            "pull" | "pull-merge" | "pull-rebase" => {
                let strategy = match action {
                    "pull-merge" => PullStrategy::Merge,
                    "pull-rebase" => PullStrategy::Rebase,
                    _ => PullStrategy::FastForwardOnly,
                };
                repo.pull(strategy, auth).map(|out| {
                    out.lines().last().unwrap_or("Pulled.").to_string()
                })
            }
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
    app.load_local_ci();
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
    app.dialog = Dialog::PullRequests;
    app.load_open_prs();
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
        .fill(theme::ember_deep().linear_multiply(0.25))
        .stroke(egui::Stroke::new(1.0_f32, theme::ember_deep()))
        .corner_radius(8.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let text = match state {
                    RepoState::Merging => "Merge in progress.".to_string(),
                    RepoState::Rebasing => {
                        // Show applied/total commits during a rebase.
                        app.repo
                            .as_ref()
                            .and_then(|r| r.rebase_progress())
                            .map(|(done, total)| {
                                format!("Rebase in progress ({done} of {total} commits).")
                            })
                            .unwrap_or_else(|| "Rebase in progress.".to_string())
                    }
                    RepoState::CherryPicking => "Cherry-pick in progress.".to_string(),
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
                    app.confirm(if state == RepoState::Merging {
                        crate::app::ConfirmAction::AbortMerge
                    } else {
                        crate::app::ConfirmAction::AbortRebase
                    });
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
        // A hard ceiling: a greedy child — a text field asking for the
        // available width, a scroll area told not to shrink — can otherwise
        // grow the panel until the viewport is a sliver.
        .width_range(280.0..=460.0)
        .frame(egui::Frame::new().fill(theme::panel()).inner_margin(8.0))
        .show(ctx, |ui| {
            // Wrapped: five tab buttons in one unwrapped row are wider than
            // the panel's default width, and a panel grows to fit its
            // content — so an unwrapped row silently widens the sidebar.
            ui.horizontal_wrapped(|ui| {
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
                // Problem count across every open buffer, so a broken file
                // is visible from any tab.
                let problems: usize = app
                    .editor
                    .files
                    .iter()
                    .map(|f| {
                        app.lsp
                            .diagnostics(&f.path)
                            .iter()
                            .filter(|d| {
                                d.severity == crate::lsp::protocol::Severity::Error
                            })
                            .count()
                    })
                    .sum();
                let dirty = app.editor.dirty_files().len();
                let editor_label = match (dirty, problems) {
                    (0, 0) => "Editor".to_string(),
                    (0, p) => format!("Editor ({p})"),
                    (d, 0) => format!("Editor ({d} unsaved)"),
                    (d, p) => format!("Editor ({d} unsaved, {p})"),
                };
                if ui.selectable_label(app.tab == Tab::Editor, editor_label).clicked() {
                    app.tab = Tab::Editor;
                }
                #[cfg(unix)]
                {
                    let running = app.terminal.running();
                    let label = match running {
                        0 => "Terminal".to_string(),
                        n => format!("Terminal ({n})"),
                    };
                    if ui.selectable_label(app.terminal.open, label).clicked() {
                        app.terminal_toggle();
                    }
                }
                let agent_label = if app.coding.running {
                    "Agent (working)".to_string()
                } else {
                    match app.coding.edits.iter().filter(|e| !e.applied).count() {
                        0 => "Agent".to_string(),
                        n => format!("Agent ({n} to review)"),
                    }
                };
                if ui.selectable_label(app.tab == Tab::Agent, agent_label).clicked() {
                    app.tab = Tab::Agent;
                }
                let checks_label = match (&app.local_ci.running, app.local_ci.history.first()) {
                    (true, _) => "Checks (running)".to_string(),
                    (false, Some(run)) if run.passed => "Checks (pass)".to_string(),
                    (false, Some(_)) => "Checks (fail)".to_string(),
                    (false, None) => "Checks".to_string(),
                };
                if ui.selectable_label(app.tab == Tab::Checks, checks_label).clicked() {
                    app.tab = Tab::Checks;
                    if !app.local_ci.running {
                        app.load_local_ci();
                    }
                }
            });
            ui.separator();
            match app.tab {
                Tab::Changes => changes_tab(app, ui),
                Tab::History => history_tab(app, ui),
                Tab::Checks => checks_tab(app, ui),
                Tab::Editor => super::editor::editor_sidebar(app, ui),
                Tab::Agent => super::agent_tab::agent_sidebar(app, ui),
            }
        });
}

fn status_glyph(status: Option<FileStatus>, conflicted: bool) -> (&'static str, Color32) {
    if conflicted {
        return ("!", theme::danger());
    }
    match status {
        Some(FileStatus::Modified) => ("M", theme::warn()),
        Some(FileStatus::Added) | Some(FileStatus::Untracked) => ("A", theme::add()),
        Some(FileStatus::Deleted) => ("D", theme::del()),
        Some(FileStatus::Renamed) => ("R", theme::teal()),
        Some(FileStatus::Copied) => ("C", theme::teal()),
        Some(FileStatus::Typechange) => ("T", theme::warn()),
        _ => ("·", theme::fg_dim()),
    }
}

/// Shortens a path to fit `max_chars`, keeping the most informative parts:
/// the filename always survives, then as many trailing directories as fit,
/// with the front elided: `…/src/app/document.rs`.
fn elide_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or(path);

    // Even the filename alone is too long: keep its end (extension matters).
    let file_len = file.chars().count();
    if file_len + 2 >= max_chars {
        let keep = max_chars.saturating_sub(1).max(1);
        let tail: String = file
            .chars()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return format!("…{tail}");
    }

    // Add trailing directories while they fit.
    let mut kept: Vec<&str> = vec![file];
    let mut used = file_len + 2; // "…/" prefix
    for dir in parts.iter().rev().skip(1) {
        let cost = dir.chars().count() + 1; // "/"
        if used + cost > max_chars {
            break;
        }
        kept.push(dir);
        used += cost;
    }
    kept.reverse();
    format!("…/{}", kept.join("/"))
}

fn changes_tab(app: &mut App, ui: &mut egui::Ui) {
    let files = app.status.as_ref().map(|s| s.files.clone()).unwrap_or_default();

    // Header: select-all checkbox, count, and gated discard-all.
    if !files.is_empty() {
        ui.horizontal(|ui| {
            let mut all = files.iter().all(|f| !app.unchecked.contains(&f.path));
            if ui
                .checkbox(&mut all, "")
                .on_hover_text("Select or deselect all files for the next commit")
                .changed()
            {
                if all {
                    app.unchecked.clear();
                } else {
                    app.unchecked = files.iter().map(|f| f.path.clone()).collect();
                }
            }
            let selected = files.iter().filter(|f| !app.unchecked.contains(&f.path)).count();
            ui.label(
                RichText::new(format!("{selected} of {} selected", files.len()))
                    .color(theme::fg_dim())
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.split.running {
                    ui.add(egui::Spinner::new().size(12.0));
                } else if files.len() > 1
                    && ui
                        .small_button("Split…")
                        .on_hover_text(
                            "Ask the AI to group these changes into separate commits, \
                             then review them before anything is committed",
                        )
                        .clicked()
                {
                    app.start_split();
                }
                if ui
                    .small_button("Discard all…")
                    .on_hover_text("Reset every change (asks first)")
                    .clicked()
                {
                    app.confirm(crate::app::ConfirmAction::DiscardAll(files.len()));
                }
            });
        });
        ui.separator();
    }

    // File list fills the space above the commit box.
    let commit_box_height = 215.0;
    let list_height = (ui.available_height() - commit_box_height).max(60.0);
    ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(
        ui,
        |ui| {
            if files.is_empty() {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No local changes").color(theme::fg_dim()));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Edit files in this repository and they will appear here.\nCtrl+Enter commits, Ctrl+R refreshes.")
                            .color(theme::fg_dim())
                            .small(),
                    );
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
                    // Fit the path to the panel: measure remaining width and
                    // convert to a character budget using the mono advance.
                    let char_width = ui.fonts(|f| {
                        f.glyph_width(&egui::TextStyle::Body.resolve(ui.style()), '0')
                    });
                    let reserved = 30.0; // discard button on the right
                    let max_chars =
                        ((ui.available_width() - reserved) / char_width).max(8.0) as usize;
                    let full = file
                        .orig_path
                        .as_ref()
                        .map(|o| format!("{o} → {}", file.path))
                        .unwrap_or_else(|| file.path.clone());
                    let display = elide_path(&full, max_chars);
                    let row = ui
                        .selectable_label(selected, RichText::new(display))
                        .on_hover_text(format!("{full}\n(double-click to edit)"));
                    if row.clicked() {
                        if selected {
                            // Clicking the viewed file again deselects it.
                            clear_diff_view(app);
                        } else {
                            select_file(app, &file.path, file.staged && !file.unstaged);
                        }
                    }
                    // Double-click opens the file in the editor, which is
                    // what every file list in every IDE does.
                    if row.double_clicked() {
                        if let Some(repo) = app.repo.clone() {
                            app.editor_open(&repo.path().join(&file.path), None);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("x")
                            .on_hover_text("Discard changes… (asks to confirm)")
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
    app.preview_text.clear();
    // A read for the previous file must not land in this one's preview.
    app.preview_loading = false;
    app.selected_commit = None;
    app.show_staged = staged;
    app.blame = None;
    app.hunks.clear();
    app.hunks_expanded = false;
    app.commit_file_list.clear();
    load_file_diff(app);
    if app.config.md_preview {
        load_preview(app);
    }
}

/// A button that shows which of two modes is active by being filled, not by
/// a tint that only reads as "selected" once you know to look for it.
fn mode_button(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let button = if active {
        egui::Button::new(RichText::new(label).color(theme::bg()).strong())
            .fill(theme::ember())
    } else {
        egui::Button::new(RichText::new(label).color(theme::fg()))
    };
    ui.add(button)
}

/// Switches between the diff and the rendered document, and remembers it.
fn set_md_preview(app: &mut App, on: bool) {
    app.config.md_preview = on;
    app.config.save();
    app.blame = None;
    if on {
        load_preview(app);
    }
}

/// Whether a path is Markdown, and so has something to render.
///
/// The extension has to be a real one: a file *named* `md`, or a dotfile
/// like `.md`, has no extension at all and is not a document.
pub fn is_markdown(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    !stem.is_empty()
        && matches!(ext.to_lowercase().as_str(), "md" | "markdown" | "mdown" | "mkd")
}

/// Cap on a previewed document. Past this the renderer is doing more work
/// than the reader wants, and the diff is the better view anyway.
const MAX_PREVIEW_BYTES: usize = 1_000_000;

/// Reads the selected file's working-tree text for the Markdown preview.
///
/// The working tree, not the index: the preview answers "what does this
/// document look like right now", which is the question someone editing
/// prose is asking. The diff beside it is where the staged/unstaged
/// distinction lives.
pub fn load_preview(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let Some(path) = app.selected_file.clone() else { return };
    if !is_markdown(&path) || app.preview_loading {
        return;
    }
    app.preview_text.clear();
    app.preview_loading = true;
    app.worker.spawn(move || {
        let full = repo.path().join(&path);
        let text = match std::fs::metadata(&full).map(|m| m.len() as usize) {
            Ok(size) if size > MAX_PREVIEW_BYTES => format!(
                "*{path} is {size} bytes — too large to render. Use the diff view.*"
            ),
            Ok(_) => match std::fs::read_to_string(&full) {
                Ok(text) => text,
                Err(e) => format!("*Cannot read {path}: {e}*"),
            },
            // A deleted file has no working-tree version to render.
            Err(_) => format!("*{path} is not in the working tree (deleted?).*"),
        };
        Msg::Preview { path, text }
    });
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
        let text = if text.trim().is_empty() {
            if repo.is_binary(&path) {
                let size = std::fs::metadata(repo.path().join(&path))
                    .map(|m| m.len())
                    .unwrap_or(0);
                format!("(binary file, {} bytes; no textual diff)", size)
            } else {
                "(no changes on this side; toggle Staged/Unstaged)".into()
            }
        } else {
            text
        };
        Msg::Diff { title: path, text }
    });
    if !staged {
        let (Some(repo), Some(path)) = (app.repo.clone(), app.selected_file.clone()) else {
            return;
        };
        app.worker.spawn(move || {
            let hunks = repo.hunks(&path).unwrap_or_default();
            Msg::Hunks { file: path, hunks }
        });
    }
}

fn discard_file(app: &mut App, path: &str) {
    app.confirm(crate::app::ConfirmAction::DiscardFile(path.to_string()));
}

/// Resets the diff viewport to its empty state.
pub fn clear_diff_view(app: &mut App) {
    app.selected_file = None;
    app.selected_commit = None;
    app.diff_title.clear();
    app.diff_text.clear();
    app.preview_text.clear();
    app.preview_loading = false;
    app.hunks.clear();
    app.hunks_expanded = false;
    app.line_sel.clear();
    app.blame = None;
    app.commit_file_list.clear();
}

fn commit_box(app: &mut App, ui: &mut egui::Ui) {
    ui.label(theme::overline("COMMIT"));
    ui.add(
        egui::TextEdit::singleline(&mut app.commit_summary)
            .hint_text(dim_hint("Summary (required)"))
            .desired_width(f32::INFINITY),
    );
    // Fixed-height, scrollable description so long text never pushes the
    // buttons below off screen.
    ScrollArea::vertical().max_height(72.0).id_salt("commit-desc").show(ui, |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut app.commit_description)
                .hint_text(dim_hint("Description"))
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
    });

    ui.horizontal(|ui| {
        ai_controls(app, ui, crate::app::worker::AiTarget::Commit, "AI message");
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
        .fill(theme::ember())
        .min_size(egui::vec2(ui.available_width(), 32.0));
    if ui.add_enabled(can_commit, commit_btn).clicked() {
        app.do_commit();
    }

    // Undo button, GitHub Desktop style: shows the last commit's summary.
    // Only offered while the commit hasn't been pushed yet (ahead > 0).
    let unpushed = app.status.as_ref().map(|s| s.ahead > 0).unwrap_or(false);
    if unpushed {
        if let Some(last) = app.log.first() {
            let subject = truncate(&last.subject, 32);
            let undo_btn = egui::Button::new(
                RichText::new(format!("Undo commit \"{subject}\"")).small(),
            )
            .min_size(egui::vec2(ui.available_width(), 24.0));
            if ui
                .add(undo_btn)
                .on_hover_text("Soft reset: removes the commit but keeps its changes staged")
                .clicked()
            {
                app.confirm(crate::app::ConfirmAction::UndoCommit(last.subject.clone()));
            }
        }
    }
}

/// Standardized AI controls: generate button + model picker with one shared
/// height and color scheme, used by both the commit box and the PR dialog.
pub fn ai_controls(
    app: &mut App,
    ui: &mut egui::Ui,
    target: crate::app::worker::AiTarget,
    label: &str,
) {
    const HEIGHT: f32 = 28.0;
    let fill = theme::teal().linear_multiply(0.25);
    let fill_hover = theme::teal().linear_multiply(0.35);

    ui.scope(|ui| {
        // One interact height and one fill for both widgets.
        ui.spacing_mut().interact_size.y = HEIGHT;
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.weak_bg_fill = fill;
        visuals.widgets.inactive.bg_fill = fill;
        visuals.widgets.hovered.weak_bg_fill = fill_hover;
        visuals.widgets.hovered.bg_fill = fill_hover;
        visuals.widgets.open.bg_fill = fill_hover;

        let enabled = !app.ai_busy && app.repo.is_some();
        let text = if app.ai_busy { "Generating…" } else { label };
        let button = egui::Button::new(text).fill(fill).min_size(egui::vec2(0.0, HEIGHT));
        // Say which context it reads: the two targets look identical but
        // describe entirely different things.
        let hint = match target {
            crate::app::worker::AiTarget::Commit => {
                "Generate from the changes you have staged"
            }
            crate::app::worker::AiTarget::PullRequest => {
                "Generate from every commit on this branch that the base does not have — \
                 not from what is staged"
            }
            _ => "Generate with the selected model",
        };
        if ui.add_enabled(enabled, button).on_hover_text(hint).clicked()
        {
            match target {
                crate::app::worker::AiTarget::Commit => app.request_ai_message(),
                crate::app::worker::AiTarget::PullRequest => app.request_pr_text(),
                // Conflict resolution and review are started from their own
                // panels; these controls only drive text generation.
                crate::app::worker::AiTarget::Conflict
                | crate::app::worker::AiTarget::Review
                | crate::app::worker::AiTarget::Coding => {}
            }
        }
        ai_model_picker(app, ui, target);
    });
}

/// Reusable AI model picker bound to one task (commit vs PR), so each task
/// can use a different provider/model (e.g. a small local model for commits,
/// a stronger Claude model for PR descriptions).
pub fn ai_model_picker(app: &mut App, ui: &mut egui::Ui, target: crate::app::worker::AiTarget) {
    use crate::app::AiSelection;
    let salt = ui.id().with("ai-model-picker");
    let current = app.ai_selection(target);
    let selected = match &current {
        Some(sel) if sel.provider == "claude" => format!("Claude: {}", sel.model),
        Some(sel) => format!("Ollama: {}", sel.model),
        None => "Select a model…".into(),
    };

    egui::ComboBox::from_id_salt(salt).selected_text(selected).show_ui(ui, |ui| {
        // Ollama section
        ui.label(theme::overline("OLLAMA (LOCAL)"));
        if app.ollama_models.is_empty() {
            ui.label(
                RichText::new("No models. Is Ollama running? (Settings)").color(theme::fg_dim()),
            );
        }
        let names: Vec<String> = app.ollama_models.iter().map(|m| m.name.clone()).collect();
        for name in names {
            let is_selected = current
                .as_ref()
                .is_some_and(|s| s.provider == "ollama" && s.model == name);
            if ui.selectable_label(is_selected, &name).clicked() {
                app.set_ai_selection(
                    target,
                    AiSelection { provider: "ollama".into(), model: name.clone() },
                );
            }
        }

        // Claude section
        ui.separator();
        ui.label(theme::overline("CLAUDE"));
        if app.claude.auth_label.is_none() {
            ui.label(RichText::new("Not signed in (Settings)").color(theme::fg_dim()));
        } else {
            let models: Vec<String> = if app.claude.models.is_empty() {
                crate::claude::FALLBACK_MODELS.iter().map(|s| s.to_string()).collect()
            } else {
                app.claude.models.clone()
            };
            for name in models {
                let is_selected = current
                    .as_ref()
                    .is_some_and(|s| s.provider == "claude" && s.model == name);
                if ui.selectable_label(is_selected, &name).clicked() {
                    app.set_ai_selection(
                        target,
                        AiSelection { provider: "claude".into(), model: name.clone() },
                    );
                }
            }
        }
    });
}

/// Uniform small control button for panel toolbars (consistent height).
pub fn panel_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).small())
            .min_size(egui::vec2(0.0, theme::CONTROL_SM)),
    )
}

/// Checks tab: live status of the current CI run plus a history of past
/// runs with per-job timing and expandable logs.
/// A push or pull request currently held by a gate, with why and a way
/// through.
///
/// Without this the decision only existed inside a modal: dismissing it lost
/// the action and there was no way back short of redoing the push. Holding it
/// here keeps the choice available without making dismissal an approval — the
/// user still has to press the button.
fn held_action_banner(app: &mut App, ui: &mut egui::Ui) {
    // Checks and the reviewer can each hold something; the checks gate runs
    // first, so prefer its message when both are set.
    enum Held {
        Checks,
        Review,
    }
    let (held, action) = if let Some(a) = app.local_ci.blocked.clone() {
        (Held::Checks, a)
    } else if app.review.pending.is_some()
        && app.review.outcome.as_ref().map(|o| o.should_block(&app.review.config)).unwrap_or(false)
    {
        (Held::Review, app.review.pending.clone().unwrap())
    } else {
        return;
    };

    let reason = match held {
        Held::Checks => {
            let names: Vec<&str> = app
                .local_ci
                .results
                .iter()
                .flatten()
                .filter(|r| !r.ok)
                .map(|r| r.name.as_str())
                .collect();
            format!("{} check(s) failed: {}", names.len(), names.join(", "))
        }
        Held::Review => {
            let o = app.review.outcome.as_ref();
            let blocking = o
                .map(|o| o.blocking(app.review.config.fail_on).len())
                .unwrap_or(0);
            match o.and_then(|o| o.markdown.as_ref()) {
                // Custom-Markdown mode has no severities to count.
                Some(_) => "the AI reviewer asked to hold this change".to_string(),
                None => format!(
                    "AI review found {blocking} finding(s) at or above \"{}\"",
                    app.review.config.fail_on.label()
                ),
            }
        }
    };

    ui.add_space(8.0);
    egui::Frame::new()
        .fill(theme::panel2())
        .stroke(egui::Stroke::new(1.0_f32, theme::danger()))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{} held", action.noun()))
                    .color(theme::danger())
                    .strong(),
            );
            ui.label(RichText::new(reason).color(theme::fg_dim()));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new(action.override_label()).strong())
                    .on_hover_text("Proceed despite the gate. The details stay on this tab.")
                    .clicked()
                {
                    match held {
                        Held::Checks => {
                            if let Some(a) = app.local_ci.blocked.take() {
                                app.toast("Overriding failed checks.", true);
                                // The reviewer is a separate gate and still applies.
                                app.gate_with_review(a);
                            }
                        }
                        Held::Review => {
                            if let Some(a) = app.review.pending.take() {
                                app.toast("Overriding the review.", true);
                                app.perform(a);
                            }
                        }
                    }
                }
                if ui.button("Discard").on_hover_text("Drop the held action").clicked() {
                    app.local_ci.blocked = None;
                    app.review.pending = None;
                    app.toast("Discarded.", false);
                }
                if matches!(held, Held::Review) && ui.button("Show findings").clicked() {
                    app.dialog = crate::app::Dialog::ReviewGate;
                }
            });
        });
    ui.add_space(4.0);
}

/// The latest AI review, kept visible after the gate dialog is dismissed so
/// the findings a user overrode are still there to come back to.
/// The files the reviewer opened, listed under the findings.
///
/// Empty when the review saw only the diff, which is itself worth knowing:
/// findings from a reviewer that could not read the code around a change
/// deserve more scepticism than ones from a reviewer that did.
fn review_context_log(ui: &mut egui::Ui, log: &[String]) {
    if log.is_empty() {
        return;
    }
    ui.add_space(4.0);
    egui::CollapsingHeader::new(format!("What the reviewer read ({} steps)", log.len()))
        .default_open(false)
        .id_salt("checks-review-context")
        .show(ui, |ui| {
            for line in log {
                ui.label(RichText::new(line).small().monospace().color(theme::fg_dim()));
            }
        });
}

fn review_section(app: &mut App, ui: &mut egui::Ui) {
    use crate::review::Severity;

    if let Some(err) = app.review.error.clone() {
        ui.add_space(8.0);
        ui.label(RichText::new(format!("AI review failed: {err}")).color(theme::danger()));
    }

    let Some(outcome) = app.review.outcome.clone() else { return };
    let (high, medium, low) = outcome.tally();
    let fail_on = app.review.config.fail_on;

    ui.add_space(10.0);

    // Markdown mode: render the reviewer's own formatting.
    if let Some(md) = outcome.markdown.clone() {
        let held = if outcome.verdict_blocks { " — reviewer asked to hold" } else { "" };
        egui::CollapsingHeader::new(RichText::new(format!("AI review{held}")).strong())
            .default_open(true)
            .show(ui, |ui| {
                super::markdown::render(ui, &md);
                review_context_log(ui, &outcome.context_log);
            });
        ui.add_space(4.0);
        return;
    }

    let header = if outcome.findings.is_empty() {
        "AI review — nothing found".to_string()
    } else {
        format!("AI review — {high} high · {medium} medium · {low} low")
    };
    egui::CollapsingHeader::new(RichText::new(header).strong())
        .default_open(!outcome.findings.is_empty())
        .show(ui, |ui| {
            if !outcome.summary.is_empty() {
                ui.label(&outcome.summary);
            }
            if !outcome.reasoning.is_empty() {
                ui.add_space(4.0);
                egui::CollapsingHeader::new("Reviewer's reasoning").default_open(false).show(
                    ui,
                    |ui| {
                        ui.label(RichText::new(&outcome.reasoning).color(theme::fg_dim()));
                    },
                );
            }
            review_context_log(ui, &outcome.context_log);
            ui.add_space(6.0);
            for (i, finding) in outcome.findings.iter().enumerate() {
                let color = match finding.severity {
                    Severity::High => theme::danger(),
                    Severity::Medium => theme::ember(),
                    Severity::Low => theme::fg_dim(),
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(finding.severity.label().to_uppercase())
                            .color(color)
                            .small()
                            .strong(),
                    );
                    if !finding.file.is_empty() {
                        let loc = match finding.line {
                            Some(l) => format!("{}:{l}", finding.file),
                            None => finding.file.clone(),
                        };
                        ui.label(RichText::new(loc).color(theme::fg_dim()).small().monospace());
                    }
                    if finding.severity >= fail_on {
                        ui.label(
                            RichText::new("blocks").color(theme::danger()).small().italics(),
                        );
                    }
                    ui.label(&finding.title);
                });
                if !finding.detail.is_empty() {
                    let expanded = app.review.expanded == Some(i);
                    if ui.small_button(if expanded { "Hide" } else { "Detail" }).clicked() {
                        app.review.expanded = if expanded { None } else { Some(i) };
                    }
                    if expanded {
                        ui.label(RichText::new(&finding.detail).color(theme::fg_dim()));
                        // The line the reviewer quoted, checked against the file
                        // before this was shown. It is what makes the finding
                        // checkable rather than something to take on faith.
                        if !finding.evidence.trim().is_empty() {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(finding.evidence.trim())
                                    .monospace()
                                    .small()
                                    .color(theme::teal()),
                            );
                        }
                    }
                }
                ui.add_space(4.0);
            }
        });
    ui.add_space(4.0);
}

fn checks_tab(app: &mut App, ui: &mut egui::Ui) {
    use crate::app::CiTrigger;

    // Controls: uniform size; reload is disabled while a run is active so
    // it can never clobber live results.
    ui.horizontal(|ui| {
        let running = app.local_ci.running;
        let run_label = if running {
            format!("Running… {}/{}", app.local_ci.finished(), app.local_ci.jobs.len())
        } else {
            "Run checks".to_string()
        };
        let can_run = !app.local_ci.jobs.is_empty() && !running;
        if panel_button(ui, &run_label, can_run).clicked() {
            app.local_ci.trigger = CiTrigger::Manual;
            app.run_local_ci();
        }
        if panel_button(ui, "Reload config", !running)
            .on_hover_text(if running {
                "Disabled while checks are running"
            } else {
                "Re-read .git-manage-ci.toml"
            })
            .clicked()
        {
            app.load_local_ci();
        }
        let reviewing = app.review.running;
        let review_label = if reviewing { "Reviewing…" } else { "AI review" };
        if panel_button(ui, review_label, !reviewing)
            .on_hover_text(
                "Ask the configured AI model to review the diff this branch \
                 would push. Reports only — nothing is blocked.",
            )
            .clicked()
        {
            app.review_now();
        }
        // The reviewer picks its own model, like every other AI task.
        // `[review] provider/model` in the repo config still wins when set.
        ai_model_picker(app, ui, crate::app::worker::AiTarget::Review);
    });

    held_action_banner(app, ui);
    review_section(app, ui);

    if app.local_ci.jobs.is_empty() {
        ui.add_space(12.0);
        ui.label(
            RichText::new(format!(
                "No checks configured.\nCreate {} in the repository root\n(see the Pull Request dialog or docs/local-ci.md).",
                crate::local_ci::CONFIG_FILE
            ))
            .color(theme::fg_dim()),
        );
        return;
    }

    // Current run (live)
    if app.local_ci.running || app.local_ci.results.iter().any(|r| r.is_some()) {
        ui.separator();
        ui.label(theme::overline("CURRENT RUN"));
        let jobs = app.local_ci.jobs.clone();
        for (i, job) in jobs.iter().enumerate() {
            ui.horizontal(|ui| {
                let (status, color) =
                    match app.local_ci.results.get(i).and_then(|r| r.as_ref()) {
                        Some(r) if r.ok => (format!("[pass {:.1}s]", r.duration_secs), theme::add()),
                        Some(r) => (format!("[fail {:.1}s]", r.duration_secs), theme::danger()),
                        None if app.local_ci.running => ("[running]".into(), theme::warn()),
                        None => ("[pending]".into(), theme::fg_dim()),
                    };
                ui.label(RichText::new(status).color(color).small().monospace());
                let expanded = app.local_ci.expanded == Some(i);
                if ui.selectable_label(expanded, job.display_name()).clicked() {
                    app.local_ci.expanded = if expanded { None } else { Some(i) };
                }
            });
            if app.local_ci.expanded == Some(i) {
                if let Some(Some(result)) = app.local_ci.results.get(i) {
                    ci_log_box(ui, i, &result.output);
                }
            }
        }
    }

    // History
    ui.separator();
    ui.label(theme::overline("RUN HISTORY"));
    if app.local_ci.history.is_empty() {
        ui.label(RichText::new("No runs yet in this session.").color(theme::fg_dim()).small());
        return;
    }
    let history_len = app.local_ci.history.len();
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("ci-history").show(ui, |ui| {
        for run_idx in 0..history_len {
            let (passed, total_secs, trigger, when, results) = {
                let run = &app.local_ci.history[run_idx];
                (run.passed, run.total_secs, run.trigger, run.when, run.results.clone())
            };
            let (badge, color) = if passed {
                ("PASS", theme::add())
            } else {
                ("FAIL", theme::danger())
            };
            let age = when.elapsed().map(format_age).unwrap_or_else(|_| "?".into());
            egui::CollapsingHeader::new(
                RichText::new(format!(
                    "{badge}  {age} ago · {} · {:.1}s",
                    trigger.label(),
                    total_secs
                ))
                .color(color)
                .small(),
            )
            .id_salt(("ci-run", run_idx))
            .show(ui, |ui| {
                for (j, result) in results.iter().enumerate() {
                    let (glyph, jcolor) = if result.ok {
                        ("[pass]", theme::add())
                    } else {
                        ("[fail]", theme::danger())
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(glyph).color(jcolor).small().monospace());
                        ui.label(RichText::new(format!(
                            "{} ({:.1}s)",
                            result.name, result.duration_secs
                        ))
                        .small());
                    });
                    if !result.ok && !result.output.is_empty() {
                        ci_log_box(ui, run_idx * 100 + j, &result.output);
                    }
                }
            });
        }
    });
}

/// Monospace log box for CI output.
fn ci_log_box(ui: &mut egui::Ui, salt: usize, output: &str) {
    ScrollArea::vertical().max_height(140.0).id_salt(("ci-log", salt)).show(ui, |ui| {
        egui::Frame::new()
            .fill(theme::bg())
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                for line in output.lines() {
                    ui.label(RichText::new(line).monospace().small());
                }
            });
    });
}

/// Rough human-readable age: "3m", "2h", "5d".
fn format_age(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Search across history, and the banner for a file-history view.
///
/// The mode is what makes this worth having. "Code" is git's pickaxe: it
/// finds the commits where a piece of text appeared or disappeared, which is
/// the question you actually have when something is gone and you want to
/// know who took it out.
fn history_search_bar(app: &mut App, ui: &mut egui::Ui) {
    use crate::git::SearchMode;

    // A file-history view says so, and offers the way back.
    if let Some(path) = app.history_file.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("History of").small().color(theme::fg_dim()));
            ui.label(RichText::new(&path).small().monospace().color(theme::ember()));
            ui.label(
                RichText::new(format!("· {} commit(s), renames followed", app.log.len()))
                    .small()
                    .color(theme::fg_dim()),
            );
            if ui.small_button("Show all history").clicked() {
                app.clear_history_filter();
            }
        });
        ui.separator();
        return;
    }

    ui.horizontal(|ui| {
        let mode = app.history_mode;
        egui::ComboBox::from_id_salt("history-mode")
            .selected_text(mode.label())
            .width(90.0)
            .show_ui(ui, |ui| {
                for option in SearchMode::ALL {
                    if ui
                        .selectable_label(mode == *option, option.label())
                        .on_hover_text(option.hint())
                        .clicked()
                        && mode != *option
                    {
                        app.history_mode = *option;
                        if !app.history_query.trim().is_empty() {
                            app.load_history();
                        }
                    }
                }
            });

        let response = ui.add(
            egui::TextEdit::singleline(&mut app.history_query)
                .hint_text(dim_hint(app.history_mode.hint()))
                .desired_width(f32::INFINITY),
        );
        // Searching history is a git call per keystroke otherwise.
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            app.load_history();
        }
    });

    // A second row: five controls abreast would widen the whole sidebar.
    ui.horizontal(|ui| {
        if ui.small_button("Search").clicked() {
            app.load_history();
        }
        if !app.history_query.is_empty() && ui.small_button("Clear").clicked() {
            app.clear_history_filter();
        }
        if ui
            .small_button("Tidy history…")
            .on_hover_text(
                "Ask the AI how this branch's commits should be folded and worded, \
                 then review the plan before anything is rewritten",
            )
            .clicked()
        {
            app.start_tidy();
        }
        if ui
            .small_button("Undo…")
            .on_hover_text(
                "Where this branch has been — go back to before a bad merge, rebase, \
                 or reset",
            )
            .clicked()
        {
            app.open_reflog();
        }
        if app.tidy.running {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });

    if !app.history_query.trim().is_empty() {
        ui.label(
            RichText::new(format!(
                "{} commit(s) across all branches",
                app.log.len()
            ))
            .small()
            .color(theme::fg_dim()),
        );
    }
    ui.separator();
}

fn history_tab(app: &mut App, ui: &mut egui::Ui) {
    history_search_bar(app, ui);
    let commits = app.log.clone();
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if commits.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No commits yet").color(theme::fg_dim()));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Make your first commit from the Changes tab.")
                        .color(theme::fg_dim())
                        .small(),
                );
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
            .color(theme::fg_dim())
            .small();
            let response = ui.selectable_label(selected, heading);
            ui.label(meta);
            ui.separator();
            // Right-click: revert (safe for pushed commits).
            response.context_menu(|ui| {
                if ui
                    .button("Revert this commit")
                    .on_hover_text("Creates a new commit that undoes this one")
                    .clicked()
                {
                    app.confirm(crate::app::ConfirmAction::RevertCommit {
                        sha: commit.sha.clone(),
                        subject: commit.subject.clone(),
                    });
                    ui.close();
                }
            });
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
        .frame(egui::Frame::new().fill(theme::bg()).inner_margin(0.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::panel2())
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let title = match app.tab {
                            Tab::Editor => "Editor",
                            Tab::Agent => "Coding agent",
                            _ if app.diff_title.is_empty() => "Select a file to view its diff",
                            _ => &app.diff_title,
                        };
                        ui.label(RichText::new(title).strong());
                        // File-level controls only when a working file is
                        // selected and the viewport is showing its diff.
                        if app.selected_file.is_some()
                            && !matches!(app.tab, Tab::Editor | Tab::Agent)
                        {
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
                                    // Markdown files can be read instead of
                                    // diffed; nothing else has a rendering.
                                    //
                                    // Two explicit buttons rather than one
                                    // toggle: a selected `selectable_label`
                                    // differs from an unselected one only by
                                    // a dim tint, and "which mode am I in?"
                                    // should never be a guess.
                                    let markdown = app
                                        .selected_file
                                        .as_deref()
                                        .is_some_and(is_markdown);
                                    if markdown {
                                        let rendered = app.config.md_preview;
                                        if mode_button(ui, "Rendered", rendered)
                                            .on_hover_text(
                                                "Read this Markdown file as a \
                                                 document",
                                            )
                                            .clicked()
                                            && !rendered
                                        {
                                            set_md_preview(app, true);
                                        }
                                        if mode_button(ui, "Diff", !rendered)
                                            .on_hover_text("Show what changed instead")
                                            .clicked()
                                            && rendered
                                        {
                                            set_md_preview(app, false);
                                        }
                                    }
                                    if ui
                                        .button("Ignore")
                                        .on_hover_text("Add this file to .gitignore")
                                        .clicked()
                                    {
                                        ignore_selected(app);
                                    }
                                    if ui
                                        .button("History")
                                        .on_hover_text(
                                            "Every commit that touched this file, \
                                             following it through renames",
                                        )
                                        .clicked()
                                    {
                                        if let Some(path) = app.selected_file.clone() {
                                            app.show_file_history(&path);
                                        }
                                    }
                                },
                            );
                        }
                    });
                });

            // The editor and the agent own the viewport when they are the
            // active tab: their content is a file and a set of diffs, and
            // neither belongs in a 340pt sidebar.
            match app.tab {
                Tab::Editor | Tab::Agent => {
                    // The diff view draws its own margins; these two need
                    // their own, or their right edge is flush with the
                    // window and the buttons there are clipped.
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| match app.tab {
                            Tab::Editor => super::editor::editor_viewport(app, ui),
                            _ => super::agent_tab::agent_viewport(app, ui),
                        });
                    return;
                }
                _ => {}
            }

            // History mode: show the commit's file list above the patch.
            if app.selected_commit.is_some() && !app.commit_file_list.is_empty() {
                commit_file_strip(app, ui);
            }

            if let Some(blame) = app.blame.clone() {
                blame_view(ui, &blame);
                return;
            }

            // Rendered Markdown replaces the diff entirely: the point is to
            // read the document, and half a document interleaved with diff
            // markers is neither.
            if app.config.md_preview
                && app.selected_file.as_deref().is_some_and(is_markdown)
            {
                markdown_view(app, ui);
                return;
            }

            // Hunk staging bar for the unstaged view.
            if app.selected_file.is_some() && !app.show_staged && !app.hunks.is_empty() {
                hunk_bar(app, ui);
                interactive_diff(app, ui);
                return;
            }

            // Plain diff: virtualized so huge diffs stay responsive.
            // Code lines get language-aware syntax colors on top of the
            // add/remove tinting. Multi-file diffs (history mode shows a
            // whole commit) switch language per file by following the
            // diff headers, so each file is highlighted correctly.
            let base_lang = app
                .selected_file
                .as_deref()
                .map(crate::app::syntax::Lang::from_path)
                .unwrap_or(crate::app::syntax::Lang::Plain);
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let lines: Vec<&str> = app.diff_text.lines().collect();
            let line_langs = crate::app::syntax::langs_per_line(&lines, base_lang);
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            // Which removed line each added line replaced, so the words that
            // actually changed can be picked out of two near-identical lines.
            let pairs = crate::app::textdiff::pair_changed_lines(&lines);
            let partners: std::collections::HashMap<usize, usize> =
                pairs.iter().map(|(removed, added)| (*added, *removed)).collect();
            ScrollArea::both().auto_shrink([false, false]).show_rows(
                ui,
                row_height,
                lines.len(),
                |ui, range| {
                    for i in range {
                        let line = lines[i];
                        let (color, bg) = diff_line_style(line);
                        // A changed line is shown against the line it
                        // replaced, with only the differing words tinted.
                        let job = match paired_lines(&lines, &pairs, &partners, i) {
                            Some((removed, added)) => {
                                let (removed_spans, added_spans) =
                                    crate::app::textdiff::changed_words(
                                        &strip_marker(removed),
                                        &strip_marker(added),
                                    );
                                let is_addition = line.starts_with('+');
                                let spans =
                                    if is_addition { added_spans } else { removed_spans };
                                word_diff_job(
                                    line_langs[i],
                                    line,
                                    color,
                                    font.clone(),
                                    &spans,
                                    is_addition,
                                )
                            }
                            None => crate::app::syntax::diff_line_job(
                                line_langs[i],
                                line,
                                color,
                                font.clone(),
                                true,
                            ),
                        };
                        match bg {
                            Some(bg) => {
                                egui::Frame::new().fill(bg).show(ui, |ui| {
                                    ui.label(job);
                                });
                            }
                            None => {
                                ui.label(job);
                            }
                        }
                    }
                },
            );
        });
}

/// The selected Markdown file, rendered.
fn markdown_view(app: &mut App, ui: &mut egui::Ui) {
    if app.preview_text.is_empty() {
        // Ask once, then wait. `preview_loading` is what stops this render
        // path from spawning a fresh read on every frame.
        load_preview(app);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0));
            ui.label(RichText::new("rendering…").color(theme::fg_dim()).small());
        });
        return;
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Working tree — this is the file as it is now, not a diff.")
                .color(theme::fg_dim())
                .small(),
        );
    });
    ui.separator();

    let text = app.preview_text.clone();
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("md-preview").show(ui, |ui| {
        // Prose is unreadable at full window width; cap the measure the way
        // a document would. `set_max_width` is what the text actually wraps
        // against, so the cap has to be set before anything is rendered.
        let width = ui.available_width().min(900.0);
        ui.set_max_width(width);
        super::markdown::render(ui, &text);
    });
}

/// Diff view with per-line checkboxes on changed lines for line staging.
fn interactive_diff(app: &mut App, ui: &mut egui::Ui) {
    let hunks = app.hunks.clone();
    let lang = app
        .selected_file
        .as_deref()
        .map(crate::app::syntax::Lang::from_path)
        .unwrap_or(crate::app::syntax::Lang::Plain);
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    ScrollArea::both().auto_shrink([false, false]).id_salt("interactive-diff").show(
        ui,
        |ui| {
            ui.add_space(4.0);
            for (hi, hunk) in hunks.iter().enumerate() {
                let (color, bg) = diff_line_style(&hunk.header);
                let _ = bg;
                ui.label(RichText::new(&hunk.header).monospace().color(color));
                for (li, line) in hunk.text.lines().skip(1).enumerate() {
                    let changed = line.starts_with('+') || line.starts_with('-');
                    let (color, bg) = diff_line_style(line);
                    ui.horizontal(|ui| {
                        if changed {
                            let key = (hi, li);
                            let mut on = app.line_sel.contains(&key);
                            if ui.checkbox(&mut on, "").on_hover_text("Select line to stage").changed() {
                                if on {
                                    app.line_sel.insert(key);
                                } else {
                                    app.line_sel.remove(&key);
                                }
                            }
                        } else {
                            ui.add_space(26.0);
                        }
                        let job = crate::app::syntax::diff_line_job(
                            lang,
                            line,
                            color,
                            font.clone(),
                            true,
                        );
                        match bg {
                            Some(bg) => {
                                egui::Frame::new().fill(bg).show(ui, |ui| {
                                    ui.label(job);
                                });
                            }
                            None => {
                                ui.label(job);
                            }
                        }
                    });
                }
            }
        },
    );
}

/// Buttons to stage hunks or the selected lines of the current file.
///
/// Files with more than [`HUNK_BAR_LIMIT`] hunks show only the first
/// `HUNK_BAR_LIMIT` buttons behind a "Show N more" toggle: a heavily edited
/// file can produce dozens, and an unbounded wrapped row of them pushes the
/// diff itself off screen.
fn hunk_bar(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::panel())
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let total = app.hunks.len();
                ui.label(
                    RichText::new(format!("{total} hunk(s):")).color(theme::fg_dim()).small(),
                );
                let collapsed = total > HUNK_BAR_LIMIT && !app.hunks_expanded;
                let shown = if collapsed { HUNK_BAR_LIMIT } else { total };
                let hunks = app.hunks.clone();
                for (i, hunk) in hunks.iter().enumerate().take(shown) {
                    if ui
                        .small_button(format!("Stage hunk {}", i + 1))
                        .on_hover_text(&hunk.header)
                        .clicked()
                    {
                        if let Some(repo) = app.repo.clone() {
                            match repo.stage_hunk(hunk) {
                                Ok(()) => {
                                    app.toast(format!("Staged hunk {}", i + 1), false);
                                    app.line_sel.clear();
                                    load_file_diff(app);
                                    app.refresh();
                                }
                                Err(e) => app.toast(e.to_string(), true),
                            }
                        }
                    }
                }
                if collapsed {
                    let hidden = total - HUNK_BAR_LIMIT;
                    if ui
                        .small_button(format!("Show {hidden} more…"))
                        .on_hover_text(format!("Show the remaining {hidden} hunk buttons"))
                        .clicked()
                    {
                        app.hunks_expanded = true;
                    }
                } else if total > HUNK_BAR_LIMIT
                    && ui
                        .small_button("Show less")
                        .on_hover_text(format!("Collapse back to the first {HUNK_BAR_LIMIT}"))
                        .clicked()
                {
                    app.hunks_expanded = false;
                }
                // Line-level staging of the checkbox selection.
                let selected = app.line_sel.len();
                if selected > 0
                    && ui
                        .small_button(format!("Stage {selected} selected line(s)"))
                        .on_hover_text("Stage only the checked lines")
                        .clicked()
                {
                    stage_selected_lines(app);
                }
            });
        });
}

/// Applies the checkbox selection as per-hunk partial patches.
fn stage_selected_lines(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let hunks = app.hunks.clone();
    let mut errors = Vec::new();
    for (hi, hunk) in hunks.iter().enumerate() {
        let lines: Vec<usize> = app
            .line_sel
            .iter()
            .filter(|(h, _)| *h == hi)
            .map(|(_, l)| *l)
            .collect();
        if lines.is_empty() {
            continue;
        }
        if let Err(e) = repo.stage_lines(hunk, &lines) {
            errors.push(e.to_string());
        }
    }
    if errors.is_empty() {
        app.toast("Selected lines staged.", false);
    } else {
        app.toast(errors.join("; "), true);
    }
    app.line_sel.clear();
    load_file_diff(app);
    app.refresh();
}

/// Horizontal strip listing files changed in the selected commit.
fn commit_file_strip(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::panel())
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} file(s):", app.commit_file_list.len()))
                        .color(theme::fg_dim())
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
                ui.label(RichText::new(&b.sha).monospace().color(theme::teal()).small());
                ui.label(
                    RichText::new(format!("{:<12}", truncate(&b.author, 12)))
                        .monospace()
                        .color(theme::fg_dim())
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

/// The `(removed, added)` pair row `i` belongs to, if it is half of one.
///
/// `pairs` maps a removed line to the added line that replaced it;
/// `partners` is the same relation the other way round, so a row can find
/// its counterpart whichever side of the change it is on.
fn paired_lines<'a>(
    lines: &[&'a str],
    pairs: &std::collections::HashMap<usize, usize>,
    partners: &std::collections::HashMap<usize, usize>,
    i: usize,
) -> Option<(&'a str, &'a str)> {
    if let Some(&added) = pairs.get(&i) {
        return lines.get(added).map(|added| (lines[i], *added));
    }
    let &removed = partners.get(&i)?;
    lines.get(removed).map(|removed| (*removed, lines[i]))
}

/// A diff line without its leading `+`/`-`/` ` marker.
fn strip_marker(line: &str) -> String {
    match line.chars().next() {
        Some('+') | Some('-') | Some(' ') => line[1..].to_string(),
        _ => line.to_string(),
    }
}

/// A diff line with the words that actually changed tinted.
///
/// Syntax colours still apply; the word highlight is a background, so the
/// two carry different information instead of competing for the same one.
fn word_diff_job(
    lang: crate::app::syntax::Lang,
    line: &str,
    color: Color32,
    font: egui::FontId,
    changed: &[(usize, usize)],
    added: bool,
) -> egui::text::LayoutJob {
    use egui::text::LayoutJob;
    use egui::TextFormat;

    if changed.is_empty() {
        return crate::app::syntax::diff_line_job(lang, line, color, font, true);
    }
    // The spans are offsets into the content; the rendered line still has
    // its marker in front.
    let shift = usize::from(matches!(line.chars().next(), Some('+') | Some('-') | Some(' ')));
    let highlight = if added {
        theme::add().linear_multiply(0.35)
    } else {
        theme::del().linear_multiply(0.35)
    };

    let mut job = LayoutJob::default();
    let mut at = 0usize;
    for span in crate::app::syntax::highlight_line(lang, line, color) {
        let (start, end) = (at, at + span.text.len());
        at = end;
        // Split each syntax span at the boundaries of the changed words.
        let mut cuts: Vec<usize> = vec![start, end];
        for (a, b) in changed {
            for edge in [a + shift, b + shift] {
                if edge > start && edge < end {
                    cuts.push(edge);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (piece_start, piece_end) = (pair[0], pair[1]);
            let inside = changed
                .iter()
                .any(|(a, b)| a + shift <= piece_start && b + shift >= piece_end);
            job.append(
                &span.text[piece_start - start..piece_end - start],
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: span.color,
                    background: if inside { highlight } else { Color32::TRANSPARENT },
                    ..Default::default()
                },
            );
        }
    }
    job
}

pub fn diff_line_style(line: &str) -> (Color32, Option<Color32>) {
    if line.starts_with("+++") || line.starts_with("---") {
        (theme::fg_dim(), None)
    } else if line.starts_with('+') {
        (theme::add(), Some(theme::add().linear_multiply(0.08)))
    } else if line.starts_with('-') {
        (theme::del(), Some(theme::del().linear_multiply(0.08)))
    } else if line.starts_with("@@") {
        (theme::teal(), Some(theme::teal().linear_multiply(0.08)))
    } else if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("commit ")
        || line.starts_with("Author")
        || line.starts_with("Date")
    {
        (theme::fg_dim(), None)
    } else {
        (theme::fg(), None)
    }
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

/// Bottom-center transient notifications. Long messages (API errors etc.)
/// wrap vertically inside a fixed max width instead of stretching across
/// the screen.
pub fn toasts(app: &mut App, ctx: &egui::Context) {
    let Some(toast) = &app.toast else { return };
    if std::time::Instant::now() > toast.until {
        app.toast = None;
        return;
    }
    let (border, color) =
        if toast.error { (theme::danger(), theme::danger()) } else { (theme::teal(), theme::fg()) };
    let max_width = (ctx.screen_rect().width() * 0.5).clamp(280.0, 560.0);
    egui::Area::new("toast".into())
        .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::panel())
                .stroke(egui::Stroke::new(1.0_f32, border))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(18, 10))
                .show(ui, |ui| {
                    ui.set_max_width(max_width);
                    ui.label(RichText::new(&toast.text).color(color));
                });
        });
}

#[cfg(test)]
mod tests {
    use super::{elide_path, is_markdown};

    #[test]
    fn short_paths_untouched() {
        assert_eq!(elide_path("src/main.rs", 40), "src/main.rs");
    }

    #[test]
    fn long_paths_keep_tail_directories() {
        let p = "very/long/nested/directory/structure/src/document.rs";
        let e = elide_path(p, 25);
        assert!(e.starts_with("…/"), "{e}");
        assert!(e.ends_with("document.rs"), "{e}");
        assert!(e.chars().count() <= 25, "{e} = {} chars", e.chars().count());
        assert!(e.contains("src/"), "should keep closest dir: {e}");
    }

    #[test]
    fn very_long_filename_keeps_extension_end() {
        let p = "a/really_extremely_unreasonably_long_file_name_indeed.rs";
        let e = elide_path(p, 20);
        assert!(e.starts_with('…'), "{e}");
        assert!(e.ends_with(".rs"), "{e}");
        assert!(e.chars().count() <= 20, "{e}");
    }

    #[test]
    fn budget_growth_adds_more_directories() {
        let p = "one/two/three/four/five/file.rs";
        let narrow = elide_path(p, 14);
        let wide = elide_path(p, 28);
        assert!(narrow.chars().count() < wide.chars().count());
        assert!(wide.contains("four/five"), "{wide}");
    }

    #[test]
    fn markdown_extensions_are_recognized() {
        for path in ["README.md", "docs/guide.markdown", "a/b/NOTES.MD", "x.mkd"] {
            assert!(is_markdown(path), "{path} should be Markdown");
        }
        for path in ["src/main.rs", "Makefile", "notes.txt", "md", "a.md.rs", ".md"] {
            assert!(!is_markdown(path), "{path} should not be Markdown");
        }
    }

    /// The diff panel renders in every state the Markdown preview can be in.
    /// A panel that panics on an empty buffer or an unloaded preview is the
    /// failure this catches.
    #[test]
    fn the_markdown_preview_renders_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@t.io"],
            vec!["config", "user.name", "T"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        std::fs::write(tmp.path().join("README.md"), "# Title\n\nSome *prose*.\n").unwrap();

        let ctx = egui::Context::default();
        let mut app = crate::app::App::new_for_test(&ctx);
        app.repo = Some(crate::git::Repo::open(tmp.path()).unwrap());
        app.selected_file = Some("README.md".into());
        app.config.md_preview = true;

        // Not loaded yet: the spinner path.
        egui::__run_test_ctx(|ctx| super::diff_panel(&mut app, ctx));

        // Loaded: the rendering path.
        app.preview_text = "# Title\n\nSome *prose* and `code`.\n\n- a\n- b\n".into();
        egui::__run_test_ctx(|ctx| super::diff_panel(&mut app, ctx));

        // Toggled off: back to the diff, with the same file selected.
        app.config.md_preview = false;
        app.diff_text = "@@ -1 +1 @@\n-old\n+new\n".into();
        egui::__run_test_ctx(|ctx| super::diff_panel(&mut app, ctx));
    }

    #[test]
    fn clearing_the_view_drops_the_preview_text() {
        let ctx = egui::Context::default();
        let mut app = crate::app::App::new_for_test(&ctx);
        app.selected_file = Some("README.md".into());
        app.preview_text = "# stale".into();

        super::clear_diff_view(&mut app);
        assert!(app.preview_text.is_empty(), "a stale preview must not outlive the selection");
        assert!(app.selected_file.is_none());
    }

    /// Clicks at `pos` in a window `width` wide and reports whether the
    /// Markdown toggle flipped.
    ///
    /// Synthetic pointer input, because "the button does nothing" is a claim
    /// about hit-testing, and hit-testing is exactly what reading the code
    /// cannot tell you.
    fn click_toggles(app: &mut crate::app::App, ctx: &egui::Context, width: f32, pos: egui::Pos2) -> bool {
        let before = app.config.md_preview;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(width, 700.0),
            )),
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| super::diff_panel(app, ctx));
        app.config.md_preview != before
    }

    /// The Diff/Rendered buttons must be clickable, including in a narrow
    /// window where the header's controls are competing for width.
    #[test]
    fn the_rendered_toggle_responds_to_a_click() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@t.io"],
            vec!["config", "user.name", "T"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        std::fs::write(tmp.path().join("notes.md"), "# Title\n").unwrap();

        let ctx = egui::Context::default();
        let mut app = crate::app::App::new_for_test(&ctx);
        let original = app.config.md_preview;
        app.repo = Some(crate::git::Repo::open(tmp.path()).unwrap());
        app.selected_file = Some("notes.md".into());
        app.diff_title = "notes.md".into();
        app.diff_text = "+# Title\n".into();
        app.config.md_preview = false;

        for width in [1400.0_f32, 900.0, 620.0] {
            // Warm-up pass so the header is laid out before anything is clicked.
            let warm = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(width, 700.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(warm, |ctx| super::diff_panel(&mut app, ctx));

            // Sweep the header row for a position that hits the toggle.
            let mut hit = None;
            'sweep: for y in [14.0_f32, 20.0, 26.0, 32.0] {
                let mut x = width - 6.0;
                while x > 40.0 {
                    if click_toggles(&mut app, &ctx, width, egui::pos2(x, y)) {
                        hit = Some((x, y));
                        break 'sweep;
                    }
                    x -= 6.0;
                }
            }
            assert!(
                hit.is_some(),
                "at {width}pt wide, no click anywhere in the header switched to Rendered"
            );
            assert!(app.config.md_preview, "the Rendered button should have turned it on");

            // The Diff button turns it back off. It is a different button, so
            // the sweep runs again rather than reusing the same position.
            let mut back = false;
            'off: for y in [14.0_f32, 20.0, 26.0, 32.0] {
                let mut x = width - 6.0;
                while x > 40.0 {
                    if click_toggles(&mut app, &ctx, width, egui::pos2(x, y)) {
                        back = true;
                        break 'off;
                    }
                    x -= 6.0;
                }
            }
            assert!(back, "at {width}pt wide, nothing in the header switched back to Diff");
            assert!(!app.config.md_preview, "the Diff button should have turned it off");
        }

        // Leave the user's saved preference as it was found.
        app.config.md_preview = original;
        app.config.save();
    }

    /// Every piece of text the frame actually painted.
    ///
    /// Asserting on state proves the toggle flipped a bool. Asserting on the
    /// painted text proves the user's view changed, which is the thing being
    /// reported.
    fn painted_text(app: &mut crate::app::App, ctx: &egui::Context, width: f32) -> String {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(width, 700.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| super::diff_panel(app, ctx));
        let mut text = String::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        text
    }

    fn collect_text(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_text(s, out);
                }
            }
            _ => {}
        }
    }

    /// Turning the toggle on must actually replace the diff with the
    /// rendered document, and turning it off must bring the diff back.
    #[test]
    fn the_toggle_changes_what_is_on_screen() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@t.io"],
            vec!["config", "user.name", "T"],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        std::fs::write(tmp.path().join("notes.md"), "# Heading One\n\nBody prose.\n").unwrap();

        let ctx = egui::Context::default();
        let mut app = crate::app::App::new_for_test(&ctx);
        let original = app.config.md_preview;
        app.repo = Some(crate::git::Repo::open(tmp.path()).unwrap());
        app.selected_file = Some("notes.md".into());
        app.diff_title = "notes.md".into();
        app.diff_text = "+# Heading One\n+\n+Body prose.\n".into();

        // Off: the diff, with its markers.
        app.config.md_preview = false;
        let diff_view = painted_text(&mut app, &ctx, 1000.0);
        assert!(diff_view.contains("+# Heading One"), "the diff should be on screen:\n{diff_view}");

        // On: the document. The preview loads on a worker, so pump messages
        // until it arrives, exactly as the running app does each frame.
        app.config.md_preview = true;
        app.preview_text.clear();
        app.preview_loading = false;
        let mut rendered = String::new();
        for _ in 0..50 {
            rendered = painted_text(&mut app, &ctx, 1000.0);
            app.handle_messages_for_test();
            if !app.preview_text.is_empty() {
                rendered = painted_text(&mut app, &ctx, 1000.0);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            rendered.contains("Heading One") && !rendered.contains("+# Heading One"),
            "the rendered document should have replaced the diff:\n{rendered}"
        );
        assert!(rendered.contains("Working tree"), "the preview banner is missing");

        // Off again: back to the diff.
        app.config.md_preview = false;
        let back = painted_text(&mut app, &ctx, 1000.0);
        assert!(back.contains("+# Heading One"), "the diff should be back:\n{back}");

        app.config.md_preview = original;
        app.config.save();
    }
}
