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
    match app.dialog {
        Dialog::None => {}
        Dialog::RepoPicker => repo_picker(app, ctx),
        Dialog::GitHub => github_dialog(app, ctx),
        Dialog::PullRequests => pull_requests(app, ctx),
        Dialog::Conflicts => conflict_resolver(app, ctx),
        Dialog::Settings => settings(app, ctx),
    }
}

fn modal(ctx: &egui::Context, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(RichText::new(title).color(theme::EMBER).strong())
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, add_contents);
}

// ---------------------------------------------------------------------------
// Repository picker
// ---------------------------------------------------------------------------

fn repo_picker(app: &mut App, ctx: &egui::Context) {
    modal(ctx, "Open repository", |ui| {
        ui.set_min_width(420.0);

        ui.label("Local path");
        ui.horizontal(|ui| {
            if ui.button("📁 Browse…").on_hover_text("Pick a folder").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Choose a repository folder")
                    .pick_folder()
                {
                    app.repo_path_input = folder.display().to_string();
                }
            }
            ui.add(
                egui::TextEdit::singleline(&mut app.repo_path_input)
                    .hint_text("/home/user/my-project")
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
            if app.repo.is_some() && ui.button("Cancel").clicked() {
                app.dialog = Dialog::None;
            }
        });

        ui.separator();
        ui.label("Clone from URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.clone_url_input)
                .hint_text("https://github.com/user/repo.git")
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.button("📁").on_hover_text("Pick destination folder").clicked() {
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
                    .hint_text("Destination directory")
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
            ui.label(RichText::new("RECENT").color(theme::EMBER).small());
            let recents = app.config.recent_repos.clone();
            for path in recents {
                if ui.link(&path).clicked() {
                    app.open_repo(&path);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// GitHub sign-in
// ---------------------------------------------------------------------------

fn github_dialog(app: &mut App, ctx: &egui::Context) {
    modal(ctx, "GitHub", |ui| {
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
                if ui.button("Close").clicked() {
                    app.dialog = Dialog::None;
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
        } else if ui.button("🌐 Start browser sign-in").clicked() {
            app.worker.spawn(|| {
                Msg::GhDeviceCode(strerr(github::device_flow_start(github::DEFAULT_CLIENT_ID)))
            });
        }

        ui.separator();
        ui.label("Or paste a personal access token (repo scope):");
        ui.add(
            egui::TextEdit::singleline(&mut app.gh.token_input)
                .password(true)
                .hint_text("ghp_… or github_pat_…")
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
            if ui.button("Close").clicked() {
                app.gh.device = None;
                app.gh.polling = false;
                app.dialog = Dialog::None;
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

fn pull_requests(app: &mut App, ctx: &egui::Context) {
    modal(ctx, "Pull requests", |ui| {
        ui.set_min_width(460.0);

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
            if ui
                .add_enabled(!app.ai_busy, egui::Button::new("✨ AI title/body"))
                .on_hover_text("Generate from the current diff with Ollama")
                .clicked()
            {
                generate_pr_text(app);
            }
            let create_enabled = !app.pr.creating
                && !app.pr.title.trim().is_empty()
                && app.pr.head != app.pr.base;
            if ui
                .add_enabled(create_enabled, egui::Button::new("Create PR").fill(theme::EMBER))
                .clicked()
            {
                create_pr(app);
            }
            if ui.button("Close").clicked() {
                app.dialog = Dialog::None;
            }
        });

        ui.separator();
        ui.label(RichText::new("OPEN PULL REQUESTS").color(theme::EMBER).small());
        if app.pr.loading {
            ui.label(RichText::new("Loading…").color(theme::FG_DIM));
        } else if app.pr.open_prs.is_empty() {
            ui.label(RichText::new("No open pull requests.").color(theme::FG_DIM));
        }
        let prs = app.pr.open_prs.clone();
        ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
            for pr in &prs {
                if ui
                    .link(format!("#{} {}  ({} → {})", pr.number, pr.title, pr.head, pr.base))
                    .clicked()
                {
                    let _ = open::that(&pr.html_url);
                }
            }
        });
    });
}

fn generate_pr_text(app: &mut App) {
    let Some(repo) = app.repo.clone() else { return };
    let Some(model) = app.config.ollama_model.clone() else {
        app.toast("No Ollama model configured. Open Settings.", true);
        return;
    };
    let url = app.effective_ollama_url();
    app.ai_busy = true;
    app.worker.spawn(move || {
        let result = (|| -> Result<ollama::CommitSuggestion, String> {
            let diff = strerr(repo.diff_for_ai())?;
            strerr(ollama::Client::new(url).commit_message(&model, &diff))
        })();
        Msg::OllamaSuggestion(result)
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

fn conflict_resolver(app: &mut App, ctx: &egui::Context) {
    modal(ctx, "Resolve conflicts", |ui| {
        ui.set_min_width(680.0);

        if app.conflicts.files.is_empty() {
            ui.label(RichText::new("All conflicts resolved 🎉").color(theme::ADD));
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
            let marker = if resolved { "✔" } else { "⚠" };
            let text = RichText::new(format!("{marker} {path}")).color(if resolved {
                theme::ADD
            } else {
                theme::WARN
            });
            if ui.selectable_label(app.conflicts.selected == Some(*i), text).clicked() {
                app.conflicts.selected = Some(*i);
                app.conflicts.editor = app.conflicts.files[*i]
                    .working
                    .clone()
                    .unwrap_or_default();
            }
        }

        // Editor for the selected file
        if let Some(i) = app.conflicts.selected {
            let path = app.conflicts.files[i].path.clone();
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
            });
            ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut app.conflicts.editor)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(14),
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
            if ui.button("Close").clicked() {
                app.dialog = Dialog::None;
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

fn settings(app: &mut App, ctx: &egui::Context) {
    modal(ctx, "Settings", |ui| {
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
            if ui.button("Close").clicked() {
                app.config.ollama_url = Some(app.ollama_url_input.trim().to_string());
                app.config.save();
                app.dialog = Dialog::None;
            }
        });

        if !app.ollama_models.is_empty() {
            ui.label(
                RichText::new(format!("Connected. {} model(s) available.", app.ollama_models.len()))
                    .color(theme::ADD),
            );
        }
    });
}
