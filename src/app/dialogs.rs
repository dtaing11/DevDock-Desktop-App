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
        Dialog::Settings => settings(app, ctx, &mut open),
        Dialog::AddRemote => add_remote(app, ctx, &mut open),
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
fn modal(
    ctx: &egui::Context,
    title: &str,
    open: &mut bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Window::new(RichText::new(title).color(theme::EMBER).strong())
        .collapsible(false)
        .resizable(false)
        .open(open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, add_contents);
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
        });

        ui.separator();
        ui.label("Clone from URL");
        ui.add(
            egui::TextEdit::singleline(&mut app.clone_url_input)
                .hint_text("https://github.com/user/repo.git")
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

        // Clone from the signed-in GitHub account.
        if app.gh.user.is_some() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("YOUR GITHUB REPOSITORIES").color(theme::EMBER).small());
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
        });
    });
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

fn pull_requests(app: &mut App, ctx: &egui::Context, open: &mut bool) {
    modal(ctx, "Pull requests", open, |ui| {
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
        ui.label(RichText::new("OPEN PULL REQUESTS").color(theme::EMBER).small());
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
            });

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

            ui.label(
                RichText::new("MERGED RESULT (edit freely, then Save manual edit)")
                    .color(theme::EMBER)
                    .small(),
            );
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
        ui.label(RichText::new("AI PROVIDER FOR COMMIT MESSAGES").color(theme::EMBER).small());
        ui.horizontal(|ui| {
            let provider = app.config.ai_provider.clone().unwrap_or_else(|| "ollama".into());
            if ui.selectable_label(provider == "ollama", "Ollama (local)").clicked() {
                app.config.ai_provider = Some("ollama".into());
                app.config.save();
            }
            let claude_ready = app.claude.auth_label.is_some();
            let resp = ui.add_enabled(
                claude_ready,
                egui::SelectableLabel::new(provider == "claude", "Claude"),
            );
            if resp.clicked() {
                app.config.ai_provider = Some("claude".into());
                app.config.save();
            }
            if !claude_ready {
                resp.on_hover_text("Sign in to Claude below first");
            }
        });
    });
}

/// Keyboard shortcut editor: click a binding, press the new keys.
fn shortcut_settings(app: &mut App, ui: &mut egui::Ui, ctx: &egui::Context) {
    use crate::app::shortcuts::{self, Action};
    ui.label(RichText::new("KEYBOARD SHORTCUTS").color(theme::EMBER).small());

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
    ui.label(RichText::new("CUSTOM AI INSTRUCTIONS (THIS REPOSITORY)").color(theme::EMBER).small());

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
    fn prompt_slot(
        ui: &mut egui::Ui,
        label: &str,
        hint: &str,
        text: &mut String,
        file: &mut Option<String>,
        repo_root: &std::path::Path,
        changed: &mut bool,
        error: &mut Option<String>,
    ) {
        ui.label(RichText::new(label).color(theme::FG_DIM).small());
        *changed |= ui
            .add(
                egui::TextEdit::multiline(text)
                    .hint_text(hint)
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
        "Commit messages",
        "e.g. Prefix the summary with the JIRA ticket from the branch name.",
        &mut prompts.commit,
        &mut prompts.commit_file,
        &repo_root,
        &mut changed,
        &mut error,
    );
    prompt_slot(
        ui,
        "Pull request title/description",
        "e.g. Include a Testing section listing manual steps.",
        &mut prompts.pull_request,
        &mut prompts.pull_request_file,
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

/// Claude account section inside Settings: OAuth sign-in or API key.
fn claude_settings(app: &mut App, ui: &mut egui::Ui) {
    use crate::claude;
    ui.label(RichText::new("CLAUDE ACCOUNT").color(theme::EMBER).small());

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
        // Model picker (models fetched from the account's /v1/models)
        ui.horizontal(|ui| {
            ui.label("Model");
            let selected = app
                .config
                .claude_model
                .clone()
                .unwrap_or_else(|| claude::DEFAULT_MODEL.to_string());
            let models: Vec<String> = if app.claude.models.is_empty() {
                claude::FALLBACK_MODELS.iter().map(|s| s.to_string()).collect()
            } else {
                app.claude.models.clone()
            };
            egui::ComboBox::from_id_salt("claude-model").selected_text(selected).show_ui(
                ui,
                |ui| {
                    for name in &models {
                        let is_sel = app.config.claude_model.as_deref() == Some(name.as_str());
                        if ui.selectable_label(is_sel, name).clicked() {
                            app.config.claude_model = Some(name.clone());
                            app.config.save();
                        }
                    }
                },
            );
            if ui.small_button("Refresh").on_hover_text("Reload available models").clicked() {
                app.load_claude_models();
            }
        });
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
                    .hint_text("code or code#state")
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
                .hint_text("or paste an API key (sk-ant-…)")
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
                .hint_text("https://github.com/user/repo.git")
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
