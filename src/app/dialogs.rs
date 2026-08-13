//! Modal dialogs: repository picker, GitHub sign-in, pull requests,
//! conflict resolver, and settings.

use super::theme;
use super::views::origin_slug;
use super::worker::{strerr, Msg};
use super::{App, Dialog};
use crate::git::Resolution;
use crate::github;
use crate::ollama;
use egui::{RichText, ScrollArea};

/// Renders whichever dialog is open.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let mut open = true;
    match app.dialog {
        Dialog::None => {}
        Dialog::RepoPicker => repo_picker(app, ctx, &mut open),
        Dialog::GitHub => github_dialog(app, ctx, &mut open),
        Dialog::PullRequests => pull_requests(app, ctx, &mut open),
        Dialog::Conflicts => conflict_resolver(app, ctx, &mut open),
        Dialog::CiConfigReview => ci_config_review(app, ctx, &mut open),
        Dialog::PrReview => pr_review(app, ctx, &mut open),
        Dialog::Settings => settings(app, ctx, &mut open),
        Dialog::AddRemote => add_remote(app, ctx, &mut open),
        Dialog::SwitchBranch(_) => switch_branch(app, ctx, &mut open),
        Dialog::Confirm(_) => confirm_dialog(app, ctx, &mut open),
    }
    // The window's X button was clicked.
    if !open {
        if app.dialog == Dialog::GitHub {
            app.gh.device = None;
            app.gh.polling = false;
        }
        if app.dialog == Dialog::Settings {
            app.config.ollama_url = Some(app.ollama_url_input.trim().to_string());
            app.config.save();
        }
        app.dialog = Dialog::None;
    }
}

/// Modal window with a title bar and an X close button.
///
/// `open` is set to `false` when the user clicks the X; callers translate
/// that into closing the dialog (plus any cleanup).
///
/// The window is constrained to the app viewport: on small windows the
/// content scrolls inside the dialog instead of overflowing off-screen.
fn modal(
    ctx: &egui::Context,
    title: &str,
    open: &mut bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let screen = ctx.screen_rect();
    let max_height = (screen.height() - 60.0).max(200.0);
    let max_width = (screen.width() - 40.0).max(280.0);
    egui::Window::new(RichText::new(title).strong())
        .collapsible(false)
        .resizable(false)
        .open(open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .max_width(max_width)
        .max_height(max_height)
        .show(ctx, |ui| {
            ScrollArea::vertical()
                .max_height(max_height - 40.0)
                .auto_shrink([false, true])
                .show(ui, add_contents);
        });
}

// ---------------------------------------------------------------------------
// Repository picker
// ---------------------------------------------------------------------------

fn repo_picker(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Open repository", open, |ui| {
        ui.set_min_width(420.0);

        ui.label("Local path");
        ui.horizontal(|ui| {
            if ui.button("Browse…").on_hover_text("Pick a folder").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Choose a repository folder")
                    .pick_folder()
                {
                    app.repo_path_input = folder.display().to_string();
                }
            }
            ui.add(
                egui::TextEdit::singleline(&mut app.repo_path_input)
                    .hint_text(super::views::dim_hint("/home/user/my-project"))
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Open").clicked() && !app.repo_path_input.trim().is_empty() {
                let path = app.repo_path_input.trim().to_string();
                app.open_repo(&path);
            }
            if ui.button("Init new").clicked() && !app.repo_path_input.trim().is_empty() {
                let path = app.repo_path_input.trim().to_string();
                app.worker.spawn(move || match crate::git::Repo::init(&path) {
                    Ok(repo) => Msg::RepoOpened(Ok(repo.path().display().to_string())),
                    Err(e) => Msg::RepoOpened(Err(e.to_string())),
                });
            }
        });

        ui.separator();
        ui.label("Clone from URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.clone_url_input)
                .hint_text(super::views::dim_hint("https://github.com/user/repo.git"))
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.button("Browse…").on_hover_text("Pick destination folder").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Choose where to clone")
                    .pick_folder()
                {
                    // Suggest <picked>/<repo-name> based on the URL.
                    let repo_name = app
                        .clone_url_input
                        .rsplit('/')
                        .next()
                        .map(|s| s.trim_end_matches(".git"))
                        .filter(|s| !s.is_empty())
                        .unwrap_or("repo");
                    app.clone_dest_input = folder.join(repo_name).display().to_string();
                }
            }
            ui.add(
                egui::TextEdit::singleline(&mut app.clone_dest_input)
                    .hint_text(super::views::dim_hint("Destination directory"))
                    .desired_width(f32::INFINITY),
            );
        });
        if ui.button("Clone").clicked() {
            let url = app.clone_url_input.trim().to_string();
            let dest = app.clone_dest_input.trim().to_string();
            if url.is_empty() || dest.is_empty() {
                app.toast("URL and destination required.", true);
            } else {
                app.toast("Cloning…", false);
                app.worker.spawn(move || match crate::git::Repo::clone(&url, &dest) {
                    Ok(repo) => Msg::RepoOpened(Ok(repo.path().display().to_string())),
                    Err(e) => Msg::RepoOpened(Err(e.to_string())),
                });
            }
        }

        if !app.config.recent_repos.is_empty() {
            ui.separator();
            ui.label(theme::overline("RECENT"));
            let recents = app.config.recent_repos.clone();
            for path in recents {
                if ui.link(&path).clicked() {
                    app.open_repo(&path);
                }
            }
        }

        // Clone from the signed-in GitHub account.
        if app.gh.user.is_some() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(theme::overline("YOUR GITHUB REPOSITORIES"));
                if ui.small_button("Load").clicked() {
                    app.gh_repos_loading = true;
                    app.worker.spawn(|| {
                        let result = crate::github::Client::from_store()
                            .ok_or_else(|| "Not signed in".to_string())
                            .and_then(|c| strerr(c.my_repos()));
                        Msg::GhRepos(result)
                    });
                }
            });
            if app.gh_repos_loading {
                ui.label(RichText::new("Loading…").color(theme::FG_DIM));
            }
            let repos = app.gh_repos.clone();
            ScrollArea::vertical().max_height(180.0).id_salt("gh-repos").show(ui, |ui| {
                for r in &repos {
                    let lock = if r.private { " (private)" } else { "" };
                    if ui.link(format!("{}{lock}", r.full_name)).clicked() {
                        // Suggest destination next to existing repos or home.
                        let base = app
                            .config
                            .recent_repos
                            .first()
                            .and_then(|p| {
                                std::path::Path::new(p)
                                    .parent()
                                    .map(|d| d.to_path_buf())
                            })
                            .unwrap_or_else(|| {
                                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
                            });
                        let name =
                            r.full_name.split('/').next_back().unwrap_or("repo").to_string();
                        app.clone_url_input = r.clone_url.clone();
                        app.clone_dest_input = base.join(name).display().to_string();
                    }
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------
// GitHub sign-in
// ---------------------------------------------------------------------------

fn github_dialog(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "GitHub", open, |ui| {
        ui.set_min_width(420.0);

        if let Some(user) = app.gh.user.clone() {
            let name = user.name.as_deref().unwrap_or("");
            ui.label(format!("Signed in as {} {name}", user.login));
            ui.horizontal(|ui| {
                if ui.button("Sign out").clicked() {
                    let _ = github::TokenStore::clear();
                    app.gh = Default::default();
                    app.toast("Signed out.", false);
                }
            });
            return;
        }

        ui.label("Sign in with your browser using a device code:");
        if let Some(device) = app.gh.device.clone() {
            ui.horizontal(|ui| {
                ui.label("1. Open");
                ui.hyperlink(&device.verification_uri);
            });
            ui.horizontal(|ui| {
                ui.label("2. Enter code:");
                ui.label(
                    RichText::new(&device.user_code)
                        .color(theme::EMBER)
                        .monospace()
                        .size(18.0),
                );
                if ui.small_button("Copy").clicked() {
                    ctx.copy_text(device.user_code.clone());
                    app.toast("Code copied.", false);
                }
            });
            ui.label(RichText::new("Waiting for authorization…").color(theme::FG_DIM));
        } else if ui.button("Start browser sign-in").clicked() {
            app.worker.spawn(|| {
                Msg::GhDeviceCode(strerr(github::device_flow_start(github::DEFAULT_CLIENT_ID)))
            });
        }

        ui.separator();
        ui.label("Or paste a personal access token (repo scope):");
        ui.add(
            egui::TextEdit::singleline(&mut app.gh.token_input)
                .password(true)
                .hint_text(super::views::dim_hint("ghp_… or github_pat_…"))
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.button("Sign in with token").clicked() && !app.gh.token_input.trim().is_empty() {
                let token = app.gh.token_input.trim().to_string();
                app.gh.token_input.clear();
                app.worker.spawn(move || {
                    let client = github::Client::new(token.clone());
                    match client.user() {
                        Ok(user) => {
                            if let Err(e) = github::TokenStore::save(&token) {
                                return Msg::GhSignedIn(Err(e.to_string()));
                            }
                            Msg::GhSignedIn(Ok(user))
                        }
                        Err(e) => Msg::GhSignedIn(Err(e.to_string())),
                    }
                });
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

fn pull_requests(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Pull requests", open, |ui| {
        ui.set_min_width(460.0);

        local_ci_panel(app, ui);
        ui.separator();

        ui.label("Title");
        ui.add(egui::TextEdit::singleline(&mut app.pr.title).desired_width(f32::INFINITY));
        ui.label("Description");
        ui.add(
            egui::TextEdit::multiline(&mut app.pr.body)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );

        let locals: Vec<String> = app
            .branches
            .as_ref()
            .map(|b| b.local.iter().map(|br| br.name.clone()).collect())
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("From");
            egui::ComboBox::from_id_salt("pr-head").selected_text(&app.pr.head).show_ui(
                ui,
                |ui| {
                    for name in &locals {
                        ui.selectable_value(&mut app.pr.head, name.clone(), name);
                    }
                },
            );
            ui.label("into");
            egui::ComboBox::from_id_salt("pr-base").selected_text(&app.pr.base).show_ui(
                ui,
                |ui| {
                    for name in &locals {
                        ui.selectable_value(&mut app.pr.base, name.clone(), name);
                    }
                },
            );
        });

        ui.horizontal(|ui| {
            super::views::ai_controls(
                app,
                ui,
                crate::app::worker::AiTarget::PullRequest,
                "AI title/body",
            );
            let create_enabled = !app.pr.creating
                && !app.pr.title.trim().is_empty()
                && app.pr.head != app.pr.base;
            if ui
                .add_enabled(create_enabled, egui::Button::new("Create PR").fill(theme::EMBER))
                .clicked()
            {
                create_pr(app);
            }
        });

        ui.separator();
        ui.label(theme::overline("OPEN PULL REQUESTS"));
        if app.pr.loading {
            ui.label(RichText::new("Loading…").color(theme::FG_DIM));
        } else if app.pr.open_prs.is_empty() {
            ui.label(RichText::new("No open pull requests.").color(theme::FG_DIM));
        }
        let prs = app.pr.open_prs.clone();
        ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
            for pr in &prs {
                ui.horizontal(|ui| {
                    if ui
                        .link(format!("#{} {}  ({} → {})", pr.number, pr.title, pr.head, pr.base))
                        .clicked()
                    {
                        let _ = open::that(&pr.html_url);
                    }
                    ui.menu_button("Merge", |ui| {
                        ui.label(
                            RichText::new(
                                "GitHub enforces repository rules and reports back",
                            )
                            .color(theme::FG_DIM)
                            .small(),
                        );
                        for (label, method) in [
                            ("Create a merge commit", "merge"),
                            ("Squash and merge", "squash"),
                            ("Rebase and merge", "rebase"),
                        ] {
                            if ui.button(label).clicked() {
                                let number = pr.number;
                                let method = method.to_string();
                                if let Some(repo) = app.repo.clone() {
                                    app.worker.spawn(move || {
                                        let result = (|| -> Result<String, String> {
                                            let client =
                                                crate::github::Client::from_store()
                                                    .ok_or("Not signed in")?;
                                            let slug = super::views::origin_slug(&repo)
                                                .ok_or("No github.com remote")?;
                                            client
                                                .merge_pull_request(&slug, number, &method)
                                                .map_err(|e| e.to_string())
                                        })();
                                        Msg::Done { message: result, refresh: true }
                                    });
                                }
                                ui.close();
                            }
                        }
                    });
                    if ui
                        .small_button("Review")
                        .on_hover_text("Open the full diff, comment, and approve or request changes")
                        .clicked()
                    {
                        app.open_pr_review(pr.clone());
                    }
                    match app.pr.mergeable.get(&pr.number) {
                        Some(Some(false)) => {
                            ui.label(
                                RichText::new("[conflicts]").color(theme::DANGER).small(),
                            )
                            .on_hover_text("This PR cannot be merged until conflicts are resolved");
                            if ui
                                .small_button("Fix conflicts")
                                .on_hover_text(format!(
                                    "Checks out {}, merges origin/{} into it, and opens \
                                     the conflict resolver. Push afterwards to update \
                                     the PR.",
                                    pr.head, pr.base
                                ))
                                .clicked()
                            {
                                app.fix_pr_conflicts(pr.head.clone(), pr.base.clone());
                            }
                        }
                        Some(Some(true)) => {
                            ui.label(
                                RichText::new("[mergeable]").color(theme::ADD).small(),
                            );
                        }
                        _ => {}
                    }
                    if let Some(checks) = app.pr.checks.get(&pr.number) {
                        use crate::github::CheckState;
                        let (label, color) = match checks.state {
                            CheckState::Passing => ("[CI passing]", theme::ADD),
                            CheckState::Failing => ("[CI failing]", theme::DANGER),
                            CheckState::Pending => ("[CI running]", theme::WARN),
                            CheckState::None => ("[no CI]", theme::FG_DIM),
                        };
                        ui.label(RichText::new(label).color(color).small()).on_hover_text(
                            format!(
                                "{} passed, {} failed, {} running",
                                checks.passed, checks.failed, checks.pending
                            ),
                        );
                    }
                });
            }
        });
    });
}

fn create_pr(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let (title, body) = (app.pr.title.trim().to_string(), app.pr.body.trim().to_string());
    let (head, base) = (app.pr.head.clone(), app.pr.base.clone());
    let token = app.gh_token();
    app.pr.creating = true;
    app.worker.spawn(move || {
        let result = (|| -> Result<github::PullRequest, String> {
            // Ensure the head branch exists on the remote first.
            repo.push(true, token.as_deref()).map_err(|e| e.to_string())?;
            let client = github::Client::from_store().ok_or("Not signed in")?;
            let slug = origin_slug(&repo).ok_or("No github.com remote found")?;
            strerr(client.create_pull_request(&slug, &title, &body, &head, &base))
        })();
        Msg::GhPrCreated(result)
    });
}

// ---------------------------------------------------------------------------
// Conflict resolver
// ---------------------------------------------------------------------------

fn conflict_resolver(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Resolve conflicts", open, |ui| {
        ui.set_min_width(680.0);

        if app.conflicts.files.is_empty() {
            ui.label(RichText::new("All conflicts resolved").color(theme::ADD));
        }

        // File list
        let files: Vec<(usize, String)> = app
            .conflicts
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.path.clone()))
            .collect();
        for (i, path) in &files {
            let resolved = app.conflicts.resolved.contains(path);
            let marker = if resolved { "[resolved]" } else { "[conflict]" };
            let text = RichText::new(format!("{marker} {path}")).color(if resolved {
                theme::ADD
            } else {
                theme::WARN
            });
            if ui.selectable_label(app.conflicts.selected == Some(*i), text).clicked() {
                app.conflicts.selected = Some(*i);
                app.conflicts.ai_proposal = None;
                app.conflicts.editor = app.conflicts.files[*i]
                    .working
                    .clone()
                    .unwrap_or_default();
            }
        }

        // Editor for the selected file
        if let Some(i) = app.conflicts.selected {
            let path = app.conflicts.files[i].path.clone();
            let ours = app.conflicts.files[i].ours.clone().unwrap_or_default();
            let theirs = app.conflicts.files[i].theirs.clone().unwrap_or_default();
            ui.separator();
            ui.label(RichText::new(&path).color(theme::EMBER).strong());
            ui.horizontal(|ui| {
                if ui.button("Take ours (current branch)").clicked() {
                    resolve(app, &path, Resolution::Ours);
                }
                if ui.button("Take theirs (incoming)").clicked() {
                    resolve(app, &path, Resolution::Theirs);
                }
                if ui.button("Save manual edit").clicked() {
                    let content = app.conflicts.editor.clone();
                    resolve(app, &path, Resolution::Manual(content));
                }
                let ai_busy_here =
                    app.conflicts.ai_busy.as_deref() == Some(path.as_str());
                if ai_busy_here {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new("AI is merging…").italics().weak());
                } else if ui
                    .button("Resolve with AI")
                    .on_hover_text(
                        "Asks the selected AI model to merge base, ours, and \
                         theirs. The result is only a proposal: you review it \
                         below and nothing is applied until you accept it.",
                    )
                    .clicked()
                {
                    app.ai_resolve_conflict(path.clone());
                }
            });

            // Pending AI proposal: requires explicit user confirmation.
            let proposal_here = app
                .conflicts
                .ai_proposal
                .as_ref()
                .is_some_and(|p| p.path == path);
            if proposal_here {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "AI PROPOSAL. Review the merged result below and edit it \
                         if needed. Nothing is applied until you accept.",
                    )
                    .color(theme::WARN)
                    .small(),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("Accept AI merge").fill(theme::EMBER))
                        .on_hover_text(
                            "Writes the reviewed content (including your edits) \
                             to the file and marks it resolved",
                        )
                        .clicked()
                    {
                        // Apply what is in the editor, so user edits to the
                        // proposal are what actually lands.
                        let content = app.conflicts.editor.clone();
                        app.conflicts.ai_proposal = None;
                        resolve(app, &path, Resolution::Manual(content));
                    }
                    if ui.button("Discard proposal").clicked() {
                        app.conflicts.ai_proposal = None;
                        app.conflicts.editor = app.conflicts.files[i]
                            .working
                            .clone()
                            .unwrap_or_default();
                    }
                });
            }

            // Side-by-side: ours | theirs (read-only context above the editor).
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("OURS (current branch)").color(theme::TEAL).small());
                ScrollArea::vertical().max_height(140.0).id_salt("ours").show(
                    &mut cols[0],
                    |ui| {
                        for line in ours.lines() {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    },
                );
                cols[1].label(RichText::new("THEIRS (incoming)").color(theme::WARN).small());
                ScrollArea::vertical().max_height(140.0).id_salt("theirs").show(
                    &mut cols[1],
                    |ui| {
                        for line in theirs.lines() {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    },
                );
            });

            let editor_label = if proposal_here {
                "AI PROPOSED MERGE (edit freely, then Accept AI merge)"
            } else {
                "MERGED RESULT (edit freely, then Save manual edit)"
            };
            ui.label(RichText::new(editor_label).color(theme::EMBER).small());
            ScrollArea::vertical().max_height(200.0).id_salt("merged").show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut app.conflicts.editor)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(10),
                );
            });
        }

        ui.separator();
        ui.horizontal(|ui| {
            let all_resolved = app.conflicts.files.len() == app.conflicts.resolved.len();
            if ui
                .add_enabled(
                    all_resolved || app.conflicts.files.is_empty(),
                    egui::Button::new("Finish (continue merge/rebase)").fill(theme::EMBER),
                )
                .clicked()
            {
                finish_conflicts(app);
            }
        });
    });
}

/// Full in-app pull-request review: changed files with diffs, inline
/// comments on any diff line, past reviews, and an approve / request
/// changes / comment verdict. Everything is drafted locally and only
/// sent to GitHub when the user clicks Submit review.
fn pr_review(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    use crate::github::parse_patch_lines;
    let Some(pr) = app.pr.review.pr.clone() else {
        app.dialog = Dialog::PullRequests;
        return;
    };
    let title = format!("Review PR #{}: {}", pr.number, pr.title);
    modal(ctx, &title, open, |ui| {
        ui.set_min_width(760.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&pr.head).color(theme::TEAL).monospace());
            ui.label(RichText::new("into").weak());
            ui.label(RichText::new(&pr.base).color(theme::TEAL).monospace());
            ui.label(RichText::new(format!("by {}", pr.user)).weak());
            if ui.small_button("Back to list").clicked() {
                app.dialog = Dialog::PullRequests;
            }
        });

        // Existing reviews summary.
        if !app.pr.review.reviews.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for review in &app.pr.review.reviews {
                    let (label, color) = match review.state.as_str() {
                        "APPROVED" => ("approved", theme::ADD),
                        "CHANGES_REQUESTED" => ("requested changes", theme::DANGER),
                        "DISMISSED" => ("dismissed", theme::FG_DIM),
                        _ => ("commented", theme::FG_DIM),
                    };
                    let chip = format!("{} {label}", review.user);
                    let resp = ui.label(RichText::new(chip).color(color).small());
                    if !review.body.trim().is_empty() {
                        resp.on_hover_text(&review.body);
                    }
                }
            });
        }

        if app.pr.review.loading {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label(RichText::new("Loading changed files…").italics().weak());
            });
            return;
        }

        ui.separator();

        // Changed files with expandable diffs.
        let file_count = app.pr.review.files.len();
        let total_add: u64 = app.pr.review.files.iter().map(|f| f.additions).sum();
        let total_del: u64 = app.pr.review.files.iter().map(|f| f.deletions).sum();
        ui.horizontal(|ui| {
            ui.label(theme::overline(&format!("{file_count} CHANGED FILES")));
            ui.label(RichText::new(format!("+{total_add}")).color(theme::ADD).small());
            ui.label(RichText::new(format!("-{total_del}")).color(theme::DEL).small());
            let pending = app.pr.review.pending.len();
            if pending > 0 {
                ui.label(
                    RichText::new(format!("{pending} pending comment(s)"))
                        .color(theme::WARN)
                        .small(),
                );
            }
        });

        ScrollArea::vertical().max_height(340.0).id_salt("pr_review_files").show(ui, |ui| {
            for fi in 0..file_count {
                let (path, status, adds, dels, patch) = {
                    let f = &app.pr.review.files[fi];
                    (f.path.clone(), f.status.clone(), f.additions, f.deletions, f.patch.clone())
                };
                let selected = app.pr.review.selected == Some(fi);
                let marker = match status.as_str() {
                    "added" => RichText::new("[A]").color(theme::ADD),
                    "removed" => RichText::new("[D]").color(theme::DEL),
                    "renamed" => RichText::new("[R]").color(theme::TEAL),
                    _ => RichText::new("[M]").color(theme::WARN),
                };
                ui.horizontal(|ui| {
                    ui.label(marker.monospace().small());
                    let text = RichText::new(&path).monospace();
                    if ui.selectable_label(selected, text).clicked() {
                        app.pr.review.selected = if selected { None } else { Some(fi) };
                        app.pr.review.comment_target = None;
                    }
                    ui.label(RichText::new(format!("+{adds}")).color(theme::ADD).small());
                    ui.label(RichText::new(format!("-{dels}")).color(theme::DEL).small());
                });

                if !selected {
                    continue;
                }
                let Some(patch) = patch else {
                    ui.label(
                        RichText::new("No text diff (binary or too large). Review on GitHub.")
                            .italics()
                            .weak(),
                    );
                    continue;
                };

                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(6.0)
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        for pl in parse_patch_lines(&patch) {
                            let (color, bg) = super::views::diff_line_style(&pl.text);
                            // Comments anchor to the new side when the line
                            // exists there, otherwise to the old side.
                            let anchor = pl
                                .new_line
                                .map(|n| (n, "RIGHT".to_string()))
                                .or(pl.old_line.map(|n| (n, "LEFT".to_string())));
                            ui.horizontal(|ui| {
                                let num = pl
                                    .new_line
                                    .or(pl.old_line)
                                    .map(|n| format!("{n:>4}"))
                                    .unwrap_or_else(|| "    ".into());
                                ui.label(RichText::new(num).monospace().weak().small());
                                let label = match bg {
                                    Some(bg) => {
                                        RichText::new(&pl.text).monospace().color(color)
                                            .background_color(bg)
                                    }
                                    None => RichText::new(&pl.text).monospace().color(color),
                                };
                                let resp = ui.label(label);
                                if let Some((line, side)) = anchor.clone() {
                                    let has_pending = app.pr.review.pending.iter().any(|c| {
                                        c.path == path && c.line == line && c.side == side
                                    });
                                    if has_pending {
                                        ui.label(
                                            RichText::new("[comment]")
                                                .color(theme::WARN)
                                                .small(),
                                        );
                                    }
                                    if (resp.hovered() || has_pending)
                                        && ui
                                            .small_button("+")
                                            .on_hover_text("Comment on this line")
                                            .clicked()
                                    {
                                        app.pr.review.comment_target =
                                            Some((fi, line, side));
                                        app.pr.review.comment_draft.clear();
                                    }
                                }
                            });

                            // Inline comment editor under the target line.
                            if let Some((tfi, tline, tside)) =
                                app.pr.review.comment_target.clone()
                            {
                                let is_here = tfi == fi
                                    && anchor.as_ref().map(|(l, s)| (*l, s.clone()))
                                        == Some((tline, tside.clone()));
                                if is_here {
                                    ui.horizontal(|ui| {
                                        ui.add_space(40.0);
                                        ui.add(
                                            egui::TextEdit::multiline(
                                                &mut app.pr.review.comment_draft,
                                            )
                                            .hint_text("Comment on this line…")
                                            .desired_rows(2)
                                            .desired_width(480.0),
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_space(40.0);
                                        let can_add =
                                            !app.pr.review.comment_draft.trim().is_empty();
                                        if ui
                                            .add_enabled(
                                                can_add,
                                                egui::Button::new("Add to review"),
                                            )
                                            .clicked()
                                        {
                                            app.pr.review.pending.push(
                                                crate::github::ReviewComment {
                                                    path: path.clone(),
                                                    line: tline,
                                                    side: tside.clone(),
                                                    body: app
                                                        .pr
                                                        .review
                                                        .comment_draft
                                                        .trim()
                                                        .to_string(),
                                                },
                                            );
                                            app.pr.review.comment_target = None;
                                        }
                                        if ui.button("Cancel").clicked() {
                                            app.pr.review.comment_target = None;
                                        }
                                    });
                                }
                            }
                        }
                    });
            }
        });

        // Pending comments list (removable before submitting).
        if !app.pr.review.pending.is_empty() {
            ui.separator();
            ui.label(theme::overline("PENDING COMMENTS (not yet submitted)"));
            let mut remove: Option<usize> = None;
            for (i, c) in app.pr.review.pending.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{}:{}", c.path, c.line))
                            .monospace()
                            .small()
                            .color(theme::TEAL),
                    );
                    ui.label(RichText::new(&c.body).small());
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                app.pr.review.pending.remove(i);
            }
        }

        ui.separator();
        ui.label(theme::overline("YOUR REVIEW"));
        ui.add(
            egui::TextEdit::multiline(&mut app.pr.review.body)
                .hint_text("Overall review summary (optional for approve/comment)…")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );

        ui.horizontal(|ui| {
            let submitting = app.pr.review.submitting;
            if submitting {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new("Submitting…").italics().weak());
            } else {
                let own_pr = app.gh.user.as_ref().map(|u| u.login == pr.user).unwrap_or(false);
                let approve = ui
                    .add_enabled(!own_pr, egui::Button::new("Approve").fill(theme::ADD.linear_multiply(0.35)))
                    .on_hover_text(if own_pr {
                        "GitHub does not allow approving your own pull request"
                    } else {
                        "Approve this pull request"
                    });
                if approve.clicked() {
                    app.submit_pr_review("APPROVE".into());
                }
                let request = ui
                    .add_enabled(
                        !own_pr,
                        egui::Button::new("Request changes")
                            .fill(theme::DANGER.linear_multiply(0.35)),
                    )
                    .on_hover_text(if own_pr {
                        "GitHub does not allow requesting changes on your own pull request"
                    } else {
                        "Block the PR until changes are made (summary required)"
                    });
                if request.clicked() {
                    if app.pr.review.body.trim().is_empty() {
                        app.toast(
                            "Request changes needs a summary explaining what to change.",
                            true,
                        );
                    } else {
                        app.submit_pr_review("REQUEST_CHANGES".into());
                    }
                }
                let can_comment = !app.pr.review.body.trim().is_empty()
                    || !app.pr.review.pending.is_empty();
                if ui
                    .add_enabled(can_comment, egui::Button::new("Comment only"))
                    .on_hover_text("Submit comments without an approval verdict")
                    .clicked()
                {
                    app.submit_pr_review("COMMENT".into());
                }
            }
        });
    });
    if app.dialog == Dialog::None && app.pr.review.pr.is_some() {
        // Dialog was closed via the X: drop the review session.
        app.pr.review = Default::default();
    }
}

/// Review dialog for an AI-drafted `.git-manage-ci.toml`. The TOML is
/// fully editable, validated live, and only written to the repository
/// when the user explicitly confirms. Cancel discards everything.
fn ci_config_review(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Review AI-generated CI config", open, |ui| {
        ui.set_min_width(640.0);
        ui.label(
            RichText::new(
                "AI PROPOSAL. Review and edit the config below. Nothing is \
                 written to your repository until you click Save.",
            )
            .color(theme::WARN)
            .small(),
        );
        ui.add_space(4.0);

        ScrollArea::vertical().max_height(360.0).id_salt("ci_ai_toml").show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut app.ci_ai_proposal)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(18),
            );
        });

        // Live validation so the user cannot save a broken file.
        let parsed: Result<crate::local_ci::Config, _> =
            toml::from_str(&app.ci_ai_proposal);
        match &parsed {
            Ok(config) => {
                let jobs = config.jobs.len();
                let on_push = if config.on_push.run {
                    if config.on_push.block_on_failure {
                        ", runs on push (blocking)"
                    } else {
                        ", runs on push (warn only)"
                    }
                } else {
                    ""
                };
                ui.label(
                    RichText::new(format!("Valid: {jobs} job(s){on_push}"))
                        .color(theme::ADD)
                        .small(),
                );
            }
            Err(e) => {
                ui.label(
                    RichText::new(format!("Invalid TOML: {e}"))
                        .color(theme::DANGER)
                        .small(),
                );
            }
        }

        let exists = app
            .repo
            .as_ref()
            .map(|r| r.path().join(crate::local_ci::CONFIG_FILE).exists())
            .unwrap_or(false);
        if exists {
            ui.label(
                RichText::new(format!(
                    "{} already exists and will be overwritten.",
                    crate::local_ci::CONFIG_FILE
                ))
                .color(theme::WARN)
                .small(),
            );
        }

        ui.separator();
        ui.horizontal(|ui| {
            let save_label = if exists {
                format!("Save (overwrite {})", crate::local_ci::CONFIG_FILE)
            } else {
                format!("Save {}", crate::local_ci::CONFIG_FILE)
            };
            if ui
                .add_enabled(parsed.is_ok(), egui::Button::new(save_label).fill(theme::EMBER))
                .clicked()
            {
                if let Some(repo) = app.repo.as_ref() {
                    let path = repo.path().join(crate::local_ci::CONFIG_FILE);
                    match std::fs::write(&path, &app.ci_ai_proposal) {
                        Ok(()) => {
                            app.toast(
                                format!("{} saved.", crate::local_ci::CONFIG_FILE),
                                false,
                            );
                            app.ci_ai_proposal.clear();
                            app.load_local_ci();
                            app.dialog = Dialog::PullRequests;
                        }
                        Err(e) => app.toast(e.to_string(), true),
                    }
                }
            }
            if ui.button("Cancel (discard proposal)").clicked() {
                app.ci_ai_proposal.clear();
                app.dialog = Dialog::PullRequests;
            }
        });
    });
}

fn resolve(app: &mut App, path: &str, resolution: Resolution) {
    let Some(repo) = app.repo.clone() else { return };
    match repo.resolve(path, &resolution) {
        Ok(()) => {
            if !app.conflicts.resolved.contains(&path.to_string()) {
                app.conflicts.resolved.push(path.to_string());
            }
            app.toast(format!("Resolved {path}"), false);
        }
        Err(e) => app.toast(e.to_string(), true),
    }
}

fn finish_conflicts(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let rebasing = app
        .status
        .as_ref()
        .map(|s| s.state == crate::git::RepoState::Rebasing)
        .unwrap_or(false);
    app.dialog = Dialog::None;
    app.worker.spawn(move || {
        let outcome = if rebasing { repo.rebase_continue() } else { repo.merge_continue() };
        Msg::MergeOutcome(outcome)
    });
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn settings(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Settings", open, |ui| {
        ui.set_min_width(420.0);

        ui.label("Ollama server URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.ollama_url_input)
                .hint_text(ollama::DEFAULT_URL)
                .desired_width(f32::INFINITY),
        );

        ui.horizontal(|ui| {
            ui.label("Model");
            let selected =
                app.config.ollama_model.clone().unwrap_or_else(|| "none found".into());
            egui::ComboBox::from_id_salt("settings-model").selected_text(selected).show_ui(
                ui,
                |ui| {
                    let names: Vec<String> =
                        app.ollama_models.iter().map(|m| m.name.clone()).collect();
                    for name in names {
                        let is_sel = app.config.ollama_model.as_deref() == Some(name.as_str());
                        if ui.selectable_label(is_sel, &name).clicked() {
                            app.config.ollama_model = Some(name);
                            app.config.save();
                        }
                    }
                },
            );
        });

        ui.horizontal(|ui| {
            if ui.button("Test connection").clicked() {
                let url = app.ollama_url_input.trim().to_string();
                app.config.ollama_url = Some(url.clone());
                app.config.save();
                app.worker
                    .spawn(move || Msg::OllamaModels(strerr(ollama::Client::new(url).models())));
                app.toast("Checking Ollama…", false);
            }
        });

        if !app.ollama_models.is_empty() {
            ui.label(
                RichText::new(format!("Connected. {} model(s) available.", app.ollama_models.len()))
                    .color(theme::ADD),
            );
        }

        ui.separator();
        claude_settings(app, ui);

        ui.separator();
        shortcut_settings(app, ui, ctx);

        ui.separator();
        repo_prompt_settings(app, ui);

        ui.separator();
        hook_settings(app, ui);

    });
}

/// Keyboard shortcut editor: click a binding, press the new keys.
fn shortcut_settings(app: &mut App, ui: &mut egui::Ui, ctx: &egui::Context) {
    use crate::app::shortcuts::{self, Action};
    ui.label(theme::overline("KEYBOARD SHORTCUTS"));

    // Capture the next key combo while rebinding.
    if let Some(action) = app.rebinding {
        let (captured, cancelled) = ctx.input(|i| {
            (shortcuts::capture(i), i.key_pressed(egui::Key::Escape))
        });
        if cancelled {
            app.rebinding = None;
        } else if let Some(binding) = captured {
            app.config.shortcuts.set(action, binding);
            app.config.save();
            app.rebinding = None;
            app.toast(format!("{} is now {}", action.label(), binding.display()), false);
        }
    }

    for action in Action::ALL {
        ui.horizontal(|ui| {
            ui.label(action.label());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let rebinding_this = app.rebinding == Some(*action);
                let text = if rebinding_this {
                    "Press keys… (Esc cancels)".to_string()
                } else {
                    app.config.shortcuts.get(*action).display()
                };
                let button = egui::Button::new(RichText::new(text).monospace().small());
                if ui
                    .add(button)
                    .on_hover_text("Click, then press the new key combination")
                    .clicked()
                {
                    app.rebinding = if rebinding_this { None } else { Some(*action) };
                }
            });
        });
    }

    ui.horizontal(|ui| {
        if ui.small_button("Reset to defaults").clicked() {
            app.config.shortcuts = Default::default();
            app.config.save();
            app.toast("Shortcuts reset.", false);
        }
        if let Some((a, b)) = app.config.shortcuts.conflict() {
            ui.label(
                RichText::new(format!("Conflict: {} and {} share a binding", a.label(), b.label()))
                    .color(theme::DANGER)
                    .small(),
            );
        }
    });
}

/// Per-repository custom AI instructions for commit and PR generation.
///
/// Stored per worktree path, so each repository can have its own style
/// rules. Instructions can be typed inline or loaded from a Markdown file
/// (re-read on every generation, so edits to the file apply immediately).
fn repo_prompt_settings(app: &mut App, ui: &mut egui::Ui) {
    ui.label(theme::overline("CUSTOM AI INSTRUCTIONS (THIS REPOSITORY)"));

    let Some(repo) = app.repo.as_ref() else {
        ui.label(RichText::new("Open a repository to customize its prompts.").color(theme::FG_DIM));
        return;
    };
    let key = repo.path().display().to_string();
    let repo_root = repo.path().to_path_buf();
    let prompts = app.config.repo_prompts.entry(key).or_default();
    let mut changed = false;
    let mut error: Option<String> = None;

    /// Opens an .md-only file picker and returns the chosen path.
    fn pick_md_file(start_dir: &std::path::Path) -> Result<Option<String>, String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a Markdown instructions file")
            .add_filter("Markdown", &["md", "markdown"])
            .set_directory(start_dir)
            .pick_file()
        else {
            return Ok(None);
        };
        // The filter guides the dialog, but verify the extension anyway.
        let is_md = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false);
        if !is_md {
            return Err("Only Markdown (.md) files are allowed.".into());
        }
        Ok(Some(path.display().to_string()))
    }

    /// One prompt slot: inline text + optional linked .md file.
    struct PromptSlot<'a> {
        label: &'a str,
        hint: &'a str,
        text: &'a mut String,
        file: &'a mut Option<String>,
    }

    fn prompt_slot(
        ui: &mut egui::Ui,
        slot: PromptSlot<'_>,
        repo_root: &std::path::Path,
        changed: &mut bool,
        error: &mut Option<String>,
    ) {
        let PromptSlot { label, hint, text, file } = slot;
        ui.label(RichText::new(label).color(theme::FG_DIM).small());
        *changed |= ui
            .add(
                egui::TextEdit::multiline(text)
                    .hint_text(super::views::dim_hint(hint))
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            )
            .changed();
        ui.horizontal(|ui| {
            match file {
                Some(path) => {
                    let name = std::path::Path::new(path.as_str())
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    let exists = std::path::Path::new(path.as_str()).exists();
                    let label_text = if exists {
                        RichText::new(format!("File: {name}")).color(theme::TEAL).small()
                    } else {
                        RichText::new(format!("File: {name} (missing)"))
                            .color(theme::DANGER)
                            .small()
                    };
                    ui.label(label_text).on_hover_text(path.as_str());
                    if ui.small_button("Change…").clicked() {
                        match pick_md_file(repo_root) {
                            Ok(Some(new_path)) => {
                                *file = Some(new_path);
                                *changed = true;
                            }
                            Ok(None) => {}
                            Err(e) => *error = Some(e),
                        }
                    }
                    if ui.small_button("Remove").clicked() {
                        *file = None;
                        *changed = true;
                    }
                }
                None => {
                    if ui
                        .small_button("Attach .md file…")
                        .on_hover_text(
                            "Use an existing Markdown file as instructions. \
                             Re-read on each generation, so edits apply immediately.",
                        )
                        .clicked()
                    {
                        match pick_md_file(repo_root) {
                            Ok(Some(new_path)) => {
                                *file = Some(new_path);
                                *changed = true;
                            }
                            Ok(None) => {}
                            Err(e) => *error = Some(e),
                        }
                    }
                }
            }
        });
    }

    prompt_slot(
        ui,
        PromptSlot {
            label: "Commit messages",
            hint: "e.g. Prefix the summary with the JIRA ticket from the branch name.",
            text: &mut prompts.commit,
            file: &mut prompts.commit_file,
        },
        &repo_root,
        &mut changed,
        &mut error,
    );
    prompt_slot(
        ui,
        PromptSlot {
            label: "Pull request title/description",
            hint: "e.g. Include a Testing section listing manual steps.",
            text: &mut prompts.pull_request,
            file: &mut prompts.pull_request_file,
        },
        &repo_root,
        &mut changed,
        &mut error,
    );
    prompt_slot(
        ui,
        PromptSlot {
            label: "Conflict resolution",
            hint: "e.g. Prefer our naming conventions; never drop test cases.",
            text: &mut prompts.conflict,
            file: &mut prompts.conflict_file,
        },
        &repo_root,
        &mut changed,
        &mut error,
    );

    if changed {
        app.config.save();
    }
    if let Some(e) = error {
        app.toast(e, true);
    }
    ui.label(
        RichText::new(
            "Inline text and file contents are both appended to the AI prompt \
             for this repository only. Saved automatically.",
        )
        .color(theme::FG_DIM)
        .small(),
    );
}

/// Local CI section at the top of the PR dialog: configured jobs, run
/// button, per-job status with expandable output, and Docker/host modes.
fn local_ci_panel(app: &mut App, ui: &mut egui::Ui) {
    use crate::local_ci;
    ui.horizontal(|ui| {
        ui.label(theme::overline("LOCAL CHECKS"));
        if app.local_ci.jobs.is_empty() {
            if ui
                .small_button("Create config")
                .on_hover_text(format!(
                    "Writes {} with an example job. Add `image = \"...\"` to run in Docker.",
                    local_ci::CONFIG_FILE
                ))
                .clicked()
            {
                if let Some(repo) = app.repo.as_ref() {
                    match local_ci::write_template(repo.path()) {
                        Ok(()) => {
                            app.toast(format!("{} created.", local_ci::CONFIG_FILE), false);
                            app.load_local_ci();
                        }
                        Err(e) => app.toast(e.to_string(), true),
                    }
                }
            }
            if app.ci_ai_busy {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new("AI is drafting…").italics().weak());
            } else if ui
                .small_button("Generate with AI")
                .on_hover_text(
                    "Scans the repo (manifests, scripts) and asks the selected \
                     AI model to draft jobs for this project. You review and \
                     edit the file before anything is written.",
                )
                .clicked()
            {
                app.generate_ci_config();
            }
        } else {
            let running = app.local_ci.running;
            let label = if running {
                format!("Running… ({}/{})", app.local_ci.finished(), app.local_ci.jobs.len())
            } else {
                "Run all checks".into()
            };
            if super::views::panel_button(ui, &label, !running).clicked() {
                app.local_ci.trigger = crate::app::CiTrigger::PullRequest;
                app.run_local_ci();
            }
            if super::views::panel_button(ui, "Reload", !running)
                .on_hover_text(if running {
                    "Disabled while checks are running"
                } else {
                    "Re-read the config file"
                })
                .clicked()
            {
                app.load_local_ci();
            }
        }
    });

    if app.local_ci.jobs.is_empty() {
        ui.label(
            RichText::new(format!(
                "No checks configured. {} defines jobs that run locally (or in a \
                 Docker container of your choice) before you create a PR.",
                local_ci::CONFIG_FILE
            ))
            .color(theme::FG_DIM)
            .small(),
        );
        return;
    }

    let jobs = app.local_ci.jobs.clone();
    for (i, job) in jobs.iter().enumerate() {
        ui.horizontal(|ui| {
            let (status, color) = match app.local_ci.results.get(i).and_then(|r| r.as_ref()) {
                Some(result) if result.ok => {
                    (format!("[pass {:.1}s]", result.duration_secs), theme::ADD)
                }
                Some(result) => (format!("[fail {:.1}s]", result.duration_secs), theme::DANGER),
                None if app.local_ci.running => ("[running]".into(), theme::WARN),
                None => ("[pending]".into(), theme::FG_DIM),
            };
            ui.label(RichText::new(status).color(color).small().monospace());
            let env = job
                .image
                .as_deref()
                .map(|img| format!(" (docker: {img})"))
                .unwrap_or_else(|| " (host)".into());
            let expanded = app.local_ci.expanded == Some(i);
            if ui
                .selectable_label(expanded, format!("{}{env}", job.name))
                .on_hover_text("Click to show/hide output")
                .clicked()
            {
                app.local_ci.expanded = if expanded { None } else { Some(i) };
            }
        });
        if app.local_ci.expanded == Some(i) {
            if let Some(Some(result)) = app.local_ci.results.get(i) {
                ScrollArea::vertical().max_height(140.0).id_salt(("ci-out", i)).show(ui, |ui| {
                    egui::Frame::new()
                        .fill(theme::BG)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            for line in result.output.lines() {
                                ui.label(RichText::new(line).monospace().small());
                            }
                        });
                });
            }
        }
    }

    if app.local_ci.any_failed() {
        ui.label(
            RichText::new("Checks failed. You can still create the PR, but consider fixing first.")
                .color(theme::DANGER)
                .small(),
        );
    } else if app.local_ci.all_passed() {
        ui.label(RichText::new("All checks passed.").color(theme::ADD).small());
    }
    if !crate::local_ci::docker_available()
        && app.local_ci.jobs.iter().any(|j| j.image.is_some())
    {
        ui.label(
            RichText::new("Docker not found: container jobs will fail until it is installed.")
                .color(theme::WARN)
                .small(),
        );
    }
}

/// Git hook integration: install/remove the pre-push local CI hook so
/// terminal pushes are gated by the same checks as in-app pushes.
fn hook_settings(app: &mut App, ui: &mut egui::Ui) {
    use crate::local_ci;
    ui.label(theme::overline("GIT PRE-PUSH HOOK"));
    let Some(repo) = app.repo.as_ref() else {
        ui.label(RichText::new("Open a repository first.").color(theme::FG_DIM));
        return;
    };
    let root = repo.path().to_path_buf();
    let installed = local_ci::hook_installed(&root);

    ui.horizontal(|ui| {
        if installed {
            ui.label(RichText::new("Installed").color(theme::ADD).small());
            if ui.small_button("Remove").clicked() {
                let hook = root.join(".git").join("hooks").join("pre-push");
                match std::fs::remove_file(&hook) {
                    Ok(()) => app.toast("pre-push hook removed.", false),
                    Err(e) => app.toast(e.to_string(), true),
                }
            }
        } else if ui.small_button("Install pre-push hook").clicked() {
            match local_ci::install_pre_push_hook(&root) {
                Ok(()) => app.toast("pre-push hook installed.", false),
                Err(e) => app.toast(e.to_string(), true),
            }
        }
    });
    ui.label(
        RichText::new(
            "Runs the local CI jobs before every `git push` from any terminal, \
             not just from this app. Failing checks abort the push.",
        )
        .color(theme::FG_DIM)
        .small(),
    );
}

/// Claude account section inside Settings: OAuth sign-in or API key.
fn claude_settings(app: &mut App, ui: &mut egui::Ui) {
    use crate::claude;
    ui.label(theme::overline("CLAUDE ACCOUNT"));

    if let Some(label) = app.claude.auth_label {
        ui.horizontal(|ui| {
            ui.label(format!("Signed in via {label}."));
            if ui.small_button("Sign out").clicked() {
                let _ = claude::CredentialStore::clear();
                app.claude = Default::default();
                if app.config.ai_provider.as_deref() == Some("claude") {
                    app.config.ai_provider = Some("ollama".into());
                    app.config.save();
                }
                app.toast("Claude signed out.", false);
            }
        });
        // Model choice lives in the AI pickers next to the AI buttons
        // (commit box and PR dialog), not here.
        return;
    }

    // OAuth flow (claude.ai Pro/Max account)
    if let Some(flow) = app.claude.flow.clone() {
        ui.label("1. Approve access in the browser tab that opened.");
        ui.horizontal(|ui| {
            ui.label("If it didn't open:");
            if ui.link("open sign-in page").clicked() {
                let _ = open::that(&flow.url);
            }
        });
        ui.label("2. Paste the code shown after approval:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.claude.code_input)
                    .hint_text(super::views::dim_hint("code or code#state"))
                    .desired_width(260.0),
            );
            if ui.button("Connect").clicked() && !app.claude.code_input.trim().is_empty() {
                match claude::finish_oauth(&flow, &app.claude.code_input) {
                    Ok(()) => {
                        app.claude = Default::default();
                        app.claude.auth_label = claude::Client::auth_label();
                        app.config.ai_provider = Some("claude".into());
                        app.config.save();
                        app.load_claude_models();
                        app.toast("Claude connected.", false);
                    }
                    Err(e) => app.toast(e.to_string(), true),
                }
            }
            if ui.button("Cancel").clicked() {
                app.claude.flow = None;
                app.claude.code_input.clear();
            }
        });
    } else if ui.button("Sign in with claude.ai account").clicked() {
        let flow = claude::start_oauth();
        let _ = open::that(&flow.url);
        app.claude.flow = Some(flow);
    }

    // API key alternative
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.claude.api_key_input)
                .password(true)
                .hint_text(super::views::dim_hint("or paste an API key (sk-ant-…)"))
                .desired_width(260.0),
        );
        if ui.button("Save key").clicked() && !app.claude.api_key_input.trim().is_empty() {
            let key = app.claude.api_key_input.trim().to_string();
            app.claude.api_key_input.clear();
            let mut creds = claude::CredentialStore::load();
            creds.api_key = Some(key);
            match claude::CredentialStore::save(&creds) {
                Ok(()) => {
                    app.claude.auth_label = claude::Client::auth_label();
                    app.config.ai_provider = Some("claude".into());
                    app.config.save();
                    app.load_claude_models();
                    app.toast("Claude API key saved.", false);
                }
                Err(e) => app.toast(e.to_string(), true),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Add remote
// ---------------------------------------------------------------------------

fn add_remote(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Publish: add a remote", open, |ui| {
        ui.set_min_width(420.0);
        ui.label("This repository has no remote yet. Add one to publish your branch:");
        ui.add(
            egui::TextEdit::singleline(&mut app.remote_url_input)
                .hint_text(super::views::dim_hint("https://github.com/user/repo.git"))
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            let can_add = !app.remote_url_input.trim().is_empty();
            if ui
                .add_enabled(can_add, egui::Button::new("Add remote and publish").fill(theme::EMBER))
                .clicked()
            {
                let url = app.remote_url_input.trim().to_string();
                if let Some(repo) = app.repo.clone() {
                    let token = app.gh_token();
                    app.dialog = Dialog::None;
                    app.worker.spawn(move || {
                        let result = repo
                            .add_remote("origin", &url)
                            .and_then(|_| repo.push(true, token.as_deref()))
                            .map(|_| "Branch published.".to_string());
                        Msg::Done { message: strerr(result), refresh: true }
                    });
                }
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Switch branch with uncommitted changes
// ---------------------------------------------------------------------------

/// Asks how to handle uncommitted changes before a branch switch:
/// bring them along, stash them, or cancel.
fn switch_branch(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    use crate::app::CheckoutMode;
    let Dialog::SwitchBranch(target) = app.dialog.clone() else { return };
    let count = app.status.as_ref().map(|s| s.files.len()).unwrap_or(0);

    modal(ctx, "Uncommitted changes", open, |ui| {
        ui.set_min_width(380.0);
        ui.label(format!(
            "You have {count} changed file{} not yet committed.",
            if count == 1 { "" } else { "s" }
        ));
        ui.label(
            RichText::new(format!("Switching to \"{target}\""))
                .color(theme::FG_DIM)
                .small(),
        );
        ui.add_space(8.0);

        let full = egui::vec2(ui.available_width(), 30.0);
        if ui
            .add(egui::Button::new("Bring changes to the new branch").min_size(full))
            .on_hover_text(
                "Keeps your edits in the working tree. Fails safely if a file \
                 would be overwritten by the switch.",
            )
            .clicked()
        {
            app.checkout_now(&target, CheckoutMode::Bring);
        }
        if ui
            .add(egui::Button::new("Stash changes and switch").min_size(full))
            .on_hover_text(
                "Saves your edits to the stash. Restore them any time from the \
                 branch menu's Stashes list.",
            )
            .clicked()
        {
            app.checkout_now(&target, CheckoutMode::Stash);
        }
        if ui.add(egui::Button::new("Cancel").min_size(full)).clicked() {
            app.dialog = Dialog::None;
        }
    });
}

// ---------------------------------------------------------------------------
// Destructive-action confirmation
// ---------------------------------------------------------------------------

/// One confirmation dialog for every destructive action: title, a plain
/// explanation of consequences, a red confirm button, and Cancel.
fn confirm_dialog(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    let Dialog::Confirm(action) = app.dialog.clone() else { return };

    // Enter confirms, Escape cancels (Escape handled by global shortcuts).
    let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

    modal(ctx, action.title(), open, |ui| {
        ui.set_min_width(360.0);

        // Merge gets a visual source -> target card so direction is obvious.
        if let crate::app::ConfirmAction::MergeInto { source, target, protected } = &action {
            egui::Frame::new()
                .fill(theme::PANEL2)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(theme::RADIUS_MD as f32)
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        branch_chip(ui, source, theme::TEAL);
                        ui.label(
                            RichText::new("  merges into  ").color(theme::FG_DIM).small(),
                        );
                        branch_chip(
                            ui,
                            target,
                            if *protected { theme::DANGER } else { theme::EMBER },
                        );
                        if *protected {
                            ui.label(
                                RichText::new(" protected").color(theme::DANGER).small(),
                            );
                        }
                    });
                });
            ui.add_space(4.0);
        }

        ui.label(action.body());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let confirm = egui::Button::new(
                RichText::new(action.verb()).color(egui::Color32::WHITE).strong(),
            )
            .fill(theme::DANGER.linear_multiply(0.85))
            .min_size(egui::vec2(0.0, theme::CONTROL_MD));
            if ui.add(confirm).clicked() || enter {
                app.execute_confirmed(action.clone());
            }
            let cancel = egui::Button::new("Cancel")
                .min_size(egui::vec2(0.0, theme::CONTROL_MD));
            if ui.add(cancel).clicked() {
                app.dialog = Dialog::None;
            }
        });
        ui.label(
            RichText::new("Enter confirms · Esc cancels")
                .color(theme::FG_DIM)
                .small(),
        );
    });
}

/// Small pill showing a branch name, colored by role.
fn branch_chip(ui: &mut egui::Ui, name: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.15))
        .stroke(egui::Stroke::new(1.0_f32, color))
        .corner_radius(999.0)
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(name).color(color).strong().monospace());
        });
}
