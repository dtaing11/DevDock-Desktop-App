//! The Git Manage desktop application (native egui, no webview).
//!
//! Module layout:
//! - [`theme`]: colors and egui style (own visual identity).
//! - [`worker`]: background thread runner and UI messages.
//! - [`views`]: toolbar, sidebar, and diff panels.
//! - [`dialogs`]: repo picker, GitHub sign-in, pull requests, conflicts, settings.
//!
//! State lives in [`App`]; long operations run on worker threads and report
//! back through [`worker::Msg`], keeping the UI responsive.

pub mod dialogs;
pub mod theme;
pub mod views;
pub mod worker;

use crate::git::{BranchList, Commit, ConflictFile, Repo, Status};
use crate::github;
use crate::ollama;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use worker::{strerr, Msg, Worker};

/// Runs the desktop app. Blocks until the window closes.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Git Manage"),
        ..Default::default()
    };
    eframe::run_native(
        "Git Manage",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
}

// ---------------------------------------------------------------------------
// Persistent config
// ---------------------------------------------------------------------------

/// User settings persisted at `~/.config/git-manage/config.json`.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    pub recent_repos: Vec<String>,
    pub ollama_url: Option<String>,
    pub ollama_model: Option<String>,
}

impl Config {
    fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("git-manage")
            .join("config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn remember_repo(&mut self, path: &str) {
        self.recent_repos.retain(|p| p != path);
        self.recent_repos.insert(0, path.to_string());
        self.recent_repos.truncate(8);
        self.save();
    }
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

/// Which sidebar tab is active.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Tab {
    Changes,
    History,
}

/// Which modal dialog is open, if any.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Dialog {
    None,
    RepoPicker,
    GitHub,
    PullRequests,
    Conflicts,
    Settings,
    /// Ask for a remote URL before the first publish.
    AddRemote,
}

/// Transient toast notification.
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// GitHub sign-in progress.
#[derive(Default)]
pub struct GhState {
    pub user: Option<github::User>,
    pub device: Option<github::DeviceCode>,
    pub last_poll: Option<Instant>,
    pub token_input: String,
    pub polling: bool,
}

/// Pull-request dialog state.
#[derive(Default)]
pub struct PrState {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub open_prs: Vec<github::PullRequest>,
    pub loading: bool,
    pub creating: bool,
}

/// Conflict-resolver dialog state.
#[derive(Default)]
pub struct ConflictState {
    pub files: Vec<ConflictFile>,
    pub selected: Option<usize>,
    pub editor: String,
    pub resolved: Vec<String>,
}

/// Top-level application state.
pub struct App {
    pub worker: Worker,
    rx: Receiver<Msg>,
    pub config: Config,

    // repository data
    pub repo: Option<Repo>,
    pub status: Option<Status>,
    pub branches: Option<BranchList>,
    pub log: Vec<Commit>,
    pub last_refresh: Instant,

    // sidebar
    pub tab: Tab,
    pub checked: std::collections::HashSet<String>,
    pub unchecked: std::collections::HashSet<String>,
    pub selected_file: Option<String>,
    pub selected_commit: Option<String>,

    // commit box
    pub commit_summary: String,
    pub commit_description: String,
    pub ai_busy: bool,

    // diff view
    pub diff_title: String,
    pub diff_text: String,

    // dialogs
    pub dialog: Dialog,
    pub repo_path_input: String,
    pub clone_url_input: String,
    pub clone_dest_input: String,
    pub remote_url_input: String,
    pub branch_filter: String,
    pub new_branch_name: String,
    pub gh: GhState,
    pub pr: PrState,
    pub conflicts: ConflictState,
    pub ollama_url_input: String,
    pub ollama_models: Vec<ollama::Model>,

    // feedback
    pub toast: Option<Toast>,
    pub busy: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let (worker, rx) = Worker::new(ctx.clone());
        let config = Config::load();
        let mut app = Self {
            worker,
            rx,
            ollama_url_input: config
                .ollama_url
                .clone()
                .unwrap_or_else(|| ollama::DEFAULT_URL.to_string()),
            config,
            repo: None,
            status: None,
            branches: None,
            log: Vec::new(),
            last_refresh: Instant::now(),
            tab: Tab::Changes,
            checked: Default::default(),
            unchecked: Default::default(),
            selected_file: None,
            selected_commit: None,
            commit_summary: String::new(),
            commit_description: String::new(),
            ai_busy: false,
            diff_title: String::new(),
            diff_text: String::new(),
            dialog: Dialog::None,
            repo_path_input: String::new(),
            clone_url_input: String::new(),
            clone_dest_input: String::new(),
            remote_url_input: String::new(),
            branch_filter: String::new(),
            new_branch_name: String::new(),
            gh: Default::default(),
            pr: Default::default(),
            conflicts: Default::default(),
            ollama_models: Vec::new(),
            toast: None,
            busy: false,
        };
        app.startup();
        app
    }

    fn startup(&mut self) {
        // Reopen the last repository, or ask for one.
        if let Some(path) = self.config.recent_repos.first().cloned() {
            self.open_repo(&path);
        } else {
            self.dialog = Dialog::RepoPicker;
        }
        // Quietly check GitHub sign-in and Ollama models.
        self.worker.spawn(|| {
            let user = github::Client::from_store().and_then(|c| c.user().ok());
            Msg::GhUser(user)
        });
        let url = self.ollama_url_input.clone();
        self.worker.spawn(move || Msg::OllamaModels(strerr(ollama::Client::new(url).models())));
    }

    // -- actions ------------------------------------------------------------

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: Instant::now() + Duration::from_secs(if error { 6 } else { 3 }),
        });
    }

    pub fn open_repo(&mut self, path: &str) {
        let path = path.to_string();
        self.worker.spawn(move || match Repo::open(&path) {
            Ok(repo) => Msg::RepoOpened(Ok(repo.path().display().to_string())),
            Err(e) => Msg::RepoOpened(Err(e.to_string())),
        });
    }

    /// Reloads status and branches (and history when that tab is open).
    pub fn refresh(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.last_refresh = Instant::now();
        {
            let repo = repo.clone();
            self.worker.spawn(move || Msg::Status(strerr(repo.status())));
        }
        {
            let repo = repo.clone();
            self.worker.spawn(move || Msg::Branches(strerr(repo.branches())));
        }
        if self.tab == Tab::History {
            self.worker.spawn(move || Msg::Log(strerr(repo.log(200, None))));
        }
    }

    /// Files that will be included in the next commit.
    pub fn files_for_commit(&self) -> Vec<String> {
        self.status
            .as_ref()
            .map(|s| {
                s.files
                    .iter()
                    .map(|f| f.path.clone())
                    .filter(|p| !self.unchecked.contains(p))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn do_commit(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let summary = self.commit_summary.trim().to_string();
        let description = self.commit_description.trim().to_string();
        let files = self.files_for_commit();
        if summary.is_empty() || files.is_empty() {
            return;
        }
        self.commit_summary.clear();
        self.commit_description.clear();
        self.unchecked.clear();
        self.selected_file = None;
        self.diff_text.clear();
        self.diff_title.clear();
        self.worker.spawn(move || {
            let result = (|| -> Result<String, crate::git::GitError> {
                repo.unstage_all().ok();
                repo.stage(&files)?;
                let sha = repo.commit(&summary, &description, false)?;
                Ok(format!("Committed {}", &sha[..7]))
            })();
            Msg::Done { message: strerr(result), refresh: true }
        });
    }

    /// Stages checked files (so the AI sees the intended diff) and asks Ollama.
    pub fn generate_ai_message(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(model) = self.config.ollama_model.clone() else {
            self.toast("No Ollama model configured. Open Settings.", true);
            return;
        };
        let url = self.effective_ollama_url();
        let files = self.files_for_commit();
        self.ai_busy = true;
        self.worker.spawn(move || {
            let result = (|| -> Result<ollama::CommitSuggestion, String> {
                if !files.is_empty() {
                    repo.unstage_all().ok();
                    strerr(repo.stage(&files))?;
                }
                let diff = strerr(repo.diff_for_ai())?;
                strerr(ollama::Client::new(url).commit_message(&model, &diff))
            })();
            Msg::OllamaSuggestion(result)
        });
    }

    pub fn effective_ollama_url(&self) -> String {
        self.config.ollama_url.clone().unwrap_or_else(|| ollama::DEFAULT_URL.to_string())
    }

    /// GitHub token for authenticated push/pull/fetch, when signed in.
    pub fn gh_token(&self) -> Option<String> {
        self.gh.user.as_ref().and_then(|_| github::TokenStore::load())
    }

    // -- message pump -------------------------------------------------------

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::RepoOpened(Ok(path)) => match Repo::open(&path) {
                Ok(repo) => {
                    self.config.remember_repo(&path);
                    self.repo = Some(repo);
                    self.dialog = Dialog::None;
                    self.status = None;
                    self.log.clear();
                    self.diff_text.clear();
                    self.diff_title.clear();
                    self.unchecked.clear();
                    self.refresh();
                }
                Err(e) => self.toast(e.to_string(), true),
            },
            Msg::RepoOpened(Err(e)) => self.toast(e, true),
            Msg::Status(Ok(status)) => self.status = Some(status),
            Msg::Status(Err(e)) => self.toast(e, true),
            Msg::Branches(Ok(branches)) => self.branches = Some(branches),
            Msg::Branches(Err(e)) => self.toast(e, true),
            Msg::Log(Ok(log)) => self.log = log,
            Msg::Log(Err(e)) => self.toast(e, true),
            Msg::Diff { title, text } => {
                self.diff_title = title;
                self.diff_text = text;
            }
            Msg::Done { message, refresh } => {
                self.busy = false;
                match message {
                    Ok(m) => {
                        if !m.is_empty() {
                            self.toast(m, false);
                        }
                    }
                    Err(e) => self.toast(e, true),
                }
                if refresh {
                    self.refresh();
                }
            }
            Msg::MergeOutcome(outcome) => {
                self.busy = false;
                if outcome.ok {
                    self.toast(
                        if outcome.message.is_empty() { "Done.".into() } else { outcome.message },
                        false,
                    );
                } else if outcome.conflict {
                    self.toast("Conflicts detected. Open the resolver.", true);
                    self.load_conflicts();
                } else {
                    self.toast(outcome.message, true);
                }
                self.refresh();
            }
            Msg::Conflicts(Ok(files)) => {
                self.conflicts = ConflictState { files, ..Default::default() };
                self.dialog = Dialog::Conflicts;
            }
            Msg::Conflicts(Err(e)) => self.toast(e, true),

            Msg::GhDeviceCode(Ok(code)) => {
                let _ = open::that(&code.verification_uri);
                self.gh.device = Some(code);
                self.gh.polling = true;
                self.gh.last_poll = Some(Instant::now());
            }
            Msg::GhDeviceCode(Err(e)) => self.toast(e, true),
            Msg::GhSignedIn(Ok(user)) => {
                self.toast(format!("Signed in as {}", user.login), false);
                self.gh.user = Some(user);
                self.gh.device = None;
                self.gh.polling = false;
                self.dialog = Dialog::None;
            }
            Msg::GhSignedIn(Err(e)) => {
                self.gh.polling = false;
                self.toast(e, true);
            }
            Msg::GhUser(user) => self.gh.user = user,
            Msg::GhPrs(result) => {
                self.pr.loading = false;
                match result {
                    Ok(prs) => self.pr.open_prs = prs,
                    Err(e) => self.toast(e, true),
                }
            }
            Msg::GhPrCreated(result) => {
                self.pr.creating = false;
                match result {
                    Ok(pr) => {
                        self.toast(format!("PR #{} created.", pr.number), false);
                        let _ = open::that(&pr.html_url);
                        self.dialog = Dialog::None;
                    }
                    Err(e) => self.toast(e, true),
                }
            }

            Msg::OllamaModels(Ok(models)) => {
                if self.config.ollama_model.is_none() {
                    self.config.ollama_model = models.first().map(|m| m.name.clone());
                    self.config.save();
                }
                self.ollama_models = models;
            }
            Msg::OllamaModels(Err(_)) => self.ollama_models.clear(),
            Msg::OllamaSuggestion(result) => {
                self.ai_busy = false;
                match result {
                    Ok(s) => {
                        self.commit_summary = s.summary;
                        self.commit_description = s.description;
                        self.toast("Commit message generated.", false);
                    }
                    Err(e) => self.toast(e, true),
                }
            }
        }
    }

    pub fn load_conflicts(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || Msg::Conflicts(strerr(repo.conflicts())));
    }

    /// Polls the GitHub device flow at the interval GitHub requested.
    fn poll_github(&mut self) {
        let Some(device) = self.gh.device.clone() else { return };
        if !self.gh.polling {
            return;
        }
        let interval = Duration::from_secs(device.interval.max(5));
        let due = self.gh.last_poll.map(|t| t.elapsed() >= interval).unwrap_or(true);
        if !due {
            return;
        }
        self.gh.last_poll = Some(Instant::now());
        self.gh.polling = false; // re-armed when the poll comes back pending
        self.worker.spawn(move || {
            match github::device_flow_poll(github::DEFAULT_CLIENT_ID, &device.device_code) {
                Ok(Some(token)) => {
                    if let Err(e) = github::TokenStore::save(&token) {
                        return Msg::GhSignedIn(Err(e.to_string()));
                    }
                    Msg::GhSignedIn(strerr(github::Client::new(token).user()))
                }
                Ok(None) => Msg::GhUser(None), // still pending; UI re-arms polling
                Err(e) => Msg::GhSignedIn(Err(e.to_string())),
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages();

        // Device-flow polling needs periodic wakeups.
        if self.gh.device.is_some() {
            if !self.gh.polling {
                self.gh.polling = true;
            }
            self.poll_github();
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // Refresh the working tree every few seconds while idle.
        if self.repo.is_some()
            && self.tab == Tab::Changes
            && self.last_refresh.elapsed() > Duration::from_secs(3)
            && self.dialog == Dialog::None
        {
            self.refresh();
        }
        ctx.request_repaint_after(Duration::from_secs(3));

        views::toolbar(self, ctx);
        views::sidebar(self, ctx);
        views::diff_panel(self, ctx);
        dialogs::show(self, ctx);
        views::toasts(self, ctx);
    }
}
