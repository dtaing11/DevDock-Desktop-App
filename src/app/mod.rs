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
pub mod shortcuts;
pub mod theme;
pub mod views;
pub mod worker;

use crate::git::{BranchList, Commit, ConflictFile, Repo, Status};
use crate::github;
use crate::ollama;
use crate::claude;
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
            .with_title("Git Manage")
            .with_icon(load_icon()),
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

/// Window/taskbar icon, embedded into the binary.
fn load_icon() -> egui::IconData {
    let png = include_bytes!("../../assets/icons/git-manage-256.png");
    eframe::icon_data::from_png_bytes(png).unwrap_or_default()
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
    /// Legacy global provider, kept as a fallback default.
    pub ai_provider: Option<String>,
    pub claude_model: Option<String>,
    /// Model used for commit messages (independent from PR text).
    pub commit_ai: Option<AiSelection>,
    /// Model used for PR title/body (may be a stronger model).
    pub pr_ai: Option<AiSelection>,
    /// Keyboard shortcuts; missing/invalid entries fall back to defaults.
    #[serde(default)]
    pub shortcuts: shortcuts::Shortcuts,
    /// Per-repository custom AI instructions, keyed by worktree root path.
    #[serde(default)]
    pub repo_prompts: std::collections::HashMap<String, RepoPrompts>,
}

/// Custom AI prompt additions for one repository.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct RepoPrompts {
    /// Appended to the system prompt for commit messages.
    #[serde(default)]
    pub commit: String,
    /// Appended to the system prompt for PR title/body generation.
    #[serde(default)]
    pub pull_request: String,
    /// Optional Markdown file whose contents are appended for commits.
    #[serde(default)]
    pub commit_file: Option<String>,
    /// Optional Markdown file whose contents are appended for PRs.
    #[serde(default)]
    pub pull_request_file: Option<String>,
}

/// A provider/model pair chosen for one AI task.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AiSelection {
    /// "ollama" or "claude".
    pub provider: String,
    pub model: String,
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

/// Claude sign-in state.
#[derive(Default)]
pub struct ClaudeState {
    /// In-progress OAuth flow awaiting the pasted code.
    pub flow: Option<claude::OAuthFlow>,
    pub code_input: String,
    pub api_key_input: String,
    /// "claude.ai account (OAuth)", "API key", or None.
    pub auth_label: Option<&'static str>,
    /// Models available to the signed-in account (from /v1/models).
    pub models: Vec<String>,
}

/// Pull-request dialog state.
#[derive(Default)]
pub struct PrState {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub open_prs: Vec<github::PullRequest>,
    /// CI check summaries keyed by PR number.
    pub checks: std::collections::HashMap<u64, github::ChecksSummary>,
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

/// Local CI run state shown in the PR dialog.
#[derive(Default)]
pub struct LocalCiState {
    /// Jobs from the repo's config file.
    pub jobs: Vec<crate::local_ci::Job>,
    /// Result slots, one per job; None while running/pending.
    pub results: Vec<Option<crate::local_ci::JobResult>>,
    pub running: bool,
    /// Which job's output is expanded.
    pub expanded: Option<usize>,
}

impl LocalCiState {
    /// All jobs finished and passed.
    pub fn all_passed(&self) -> bool {
        !self.jobs.is_empty()
            && self.results.iter().all(|r| r.as_ref().map(|x| x.ok).unwrap_or(false))
    }

    /// Any job failed.
    pub fn any_failed(&self) -> bool {
        self.results.iter().any(|r| r.as_ref().map(|x| !x.ok).unwrap_or(false))
    }

    pub fn finished(&self) -> usize {
        self.results.iter().filter(|r| r.is_some()).count()
    }
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
    /// CI checks for the current branch head, refreshed with status.
    pub branch_checks: Option<github::ChecksSummary>,
    last_checks_refresh: Option<Instant>,
    /// Last background fetch, so remote changes surface automatically.
    last_auto_fetch: Option<Instant>,

    // sidebar
    pub tab: Tab,
    pub checked: std::collections::HashSet<String>,
    pub unchecked: std::collections::HashSet<String>,
    pub selected_file: Option<String>,
    pub selected_commit: Option<String>,

    // commit box
    pub commit_summary: String,
    pub commit_description: String,
    pub amend: bool,
    pub ai_busy: bool,

    // diff view
    pub diff_title: String,
    pub diff_text: String,
    /// Hunks of the currently selected file (for partial staging).
    pub hunks: Vec<crate::git::Hunk>,
    /// Selected changed lines per hunk index (for line-level staging).
    pub line_sel: std::collections::HashSet<(usize, usize)>,
    /// Which diff side is shown for the selected file.
    pub show_staged: bool,
    /// Blame lines when blame view is active.
    pub blame: Option<Vec<crate::git::BlameLine>>,

    // history details
    pub commit_file_list: Vec<crate::git::CommitFileChange>,

    // stash / tags / github repos
    pub stashes: Vec<crate::git::StashEntry>,
    pub tags: Vec<String>,
    pub gh_repos: Vec<github::RemoteRepo>,
    pub gh_repos_loading: bool,
    pub tag_name_input: String,
    pub rename_branch_input: String,

    // dialogs
    pub dialog: Dialog,
    pub repo_path_input: String,
    pub clone_url_input: String,
    pub clone_dest_input: String,
    pub remote_url_input: String,
    pub branch_filter: String,
    pub new_branch_name: String,
    pub gh: GhState,
    pub claude: ClaudeState,
    pub pr: PrState,
    pub local_ci: LocalCiState,
    pub conflicts: ConflictState,
    pub ollama_url_input: String,
    pub ollama_models: Vec<ollama::Model>,

    // feedback
    pub toast: Option<Toast>,
    pub busy: bool,
    /// Action currently being rebound in Settings, if any.
    pub rebinding: Option<shortcuts::Action>,
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
            branch_checks: None,
            last_checks_refresh: None,
            last_auto_fetch: None,
            tab: Tab::Changes,
            checked: Default::default(),
            unchecked: Default::default(),
            selected_file: None,
            selected_commit: None,
            commit_summary: String::new(),
            commit_description: String::new(),
            amend: false,
            ai_busy: false,
            diff_title: String::new(),
            diff_text: String::new(),
            hunks: Vec::new(),
            line_sel: Default::default(),
            show_staged: false,
            blame: None,
            commit_file_list: Vec::new(),
            stashes: Vec::new(),
            tags: Vec::new(),
            gh_repos: Vec::new(),
            gh_repos_loading: false,
            tag_name_input: String::new(),
            rename_branch_input: String::new(),
            dialog: Dialog::None,
            repo_path_input: String::new(),
            clone_url_input: String::new(),
            clone_dest_input: String::new(),
            remote_url_input: String::new(),
            branch_filter: String::new(),
            new_branch_name: String::new(),
            gh: Default::default(),
            claude: Default::default(),
            pr: Default::default(),
            local_ci: Default::default(),
            conflicts: Default::default(),
            ollama_models: Vec::new(),
            toast: None,
            busy: false,
            rebinding: None,
        };
        app.startup();
        app.claude.auth_label = claude::Client::auth_label();
        app.load_claude_models();
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
        // Always load history: the Undo button needs the last commit's
        // subject even on the Changes tab.
        self.worker.spawn(move || Msg::Log(strerr(repo.log(200, None))));
        self.load_stashes();
        self.load_tags();
        self.refresh_branch_checks();
    }

    /// Background fetch every 60 seconds so ahead/behind counts (and the
    /// sync button) update when the remote gains new commits, GitHub
    /// Desktop style. Skipped while a dialog is open or an op is running.
    fn auto_fetch(&mut self) {
        let due = self
            .last_auto_fetch
            .map(|t| t.elapsed() > Duration::from_secs(60))
            .unwrap_or(true);
        if !due || self.busy || self.dialog != Dialog::None {
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        let Some(status) = self.status.as_ref() else { return };
        if !status.has_remote {
            return;
        }
        self.last_auto_fetch = Some(Instant::now());
        let token = self.gh_token();
        self.worker.spawn(move || {
            // Quiet: no toast, but refresh counts afterwards.
            let _ = repo.fetch(token.as_deref());
            Msg::Done { message: Ok(String::new()), refresh: true }
        });
    }

    /// Fetches CI check status for the current branch head from GitHub,
    /// throttled to once every 30 seconds.
    fn refresh_branch_checks(&mut self) {
        if self.gh.user.is_none() {
            return;
        }
        let throttled = self
            .last_checks_refresh
            .map(|t| t.elapsed() < Duration::from_secs(30))
            .unwrap_or(false);
        if throttled {
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        let Some(branch) = self.status.as_ref().map(|s| s.branch.clone()) else { return };
        if branch.starts_with('(') {
            return; // detached / no commits
        }
        self.last_checks_refresh = Some(Instant::now());
        self.worker.spawn(move || {
            let result = (|| -> Option<(String, github::ChecksSummary)> {
                let client = github::Client::from_store()?;
                let slug = views::origin_slug(&repo)?;
                let summary = client.checks(&slug, &branch).ok()?;
                Some((branch, summary))
            })();
            match result {
                Some((branch, summary)) => Msg::GhBranchChecks { branch, summary },
                None => Msg::Noop,
            }
        });
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
        let amend = self.amend;
        if summary.is_empty() || (files.is_empty() && !amend) {
            return;
        }
        self.commit_summary.clear();
        self.commit_description.clear();
        self.amend = false;
        self.unchecked.clear();
        self.selected_file = None;
        self.diff_text.clear();
        self.diff_title.clear();
        self.hunks.clear();
        self.worker.spawn(move || {
            let result = (|| -> Result<String, crate::git::GitError> {
                if !files.is_empty() {
                    repo.unstage_all().ok();
                    repo.stage(&files)?;
                }
                let sha = repo.commit(&summary, &description, amend)?;
                Ok(format!("{} {}", if amend { "Amended" } else { "Committed" }, &sha[..7]))
            })();
            Msg::Done { message: strerr(result), refresh: true }
        });
    }

    /// The provider/model pair for a task, falling back to the legacy
    /// global settings when the task has no explicit selection yet.
    pub fn ai_selection(&self, target: worker::AiTarget) -> Option<AiSelection> {
        let explicit = match target {
            worker::AiTarget::Commit => self.config.commit_ai.clone(),
            worker::AiTarget::PullRequest => self.config.pr_ai.clone(),
        };
        explicit.or_else(|| {
            let provider = self.config.ai_provider.clone().unwrap_or_else(|| "ollama".into());
            let model = if provider == "claude" {
                self.config.claude_model.clone()?
            } else {
                self.config.ollama_model.clone()?
            };
            Some(AiSelection { provider, model })
        })
    }

    /// Stores the selection for a task.
    pub fn set_ai_selection(&mut self, target: worker::AiTarget, sel: AiSelection) {
        match target {
            worker::AiTarget::Commit => self.config.commit_ai = Some(sel),
            worker::AiTarget::PullRequest => self.config.pr_ai = Some(sel),
        }
        self.config.save();
    }

    /// Generates a commit message into the commit box using the selected
    /// provider/model. Stages the checked files first so the AI sees the
    /// intended diff.
    pub fn generate_ai_message(&mut self) {
        let files = self.files_for_commit();
        self.generate_ai(worker::AiTarget::Commit, files);
    }

    /// Generates a PR title/body from the current diff (no restaging).
    pub fn generate_pr_text(&mut self) {
        self.generate_ai(worker::AiTarget::PullRequest, Vec::new());
    }

    /// Custom AI instructions for the current repo and task: inline text
    /// plus the contents of a linked Markdown file, when configured.
    fn repo_prompt(&self, target: worker::AiTarget) -> Option<String> {
        let repo = self.repo.as_ref()?;
        let prompts = self.config.repo_prompts.get(&repo.path().display().to_string())?;
        let (inline, file) = match target {
            worker::AiTarget::Commit => (&prompts.commit, &prompts.commit_file),
            worker::AiTarget::PullRequest => (&prompts.pull_request, &prompts.pull_request_file),
        };
        let mut parts: Vec<String> = Vec::new();
        let inline = inline.trim();
        if !inline.is_empty() {
            parts.push(inline.to_string());
        }
        if let Some(path) = file {
            match std::fs::read_to_string(path) {
                Ok(contents) if !contents.trim().is_empty() => {
                    parts.push(contents.trim().to_string());
                }
                _ => {} // missing/unreadable file: fall back to inline text only
            }
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Shared AI generation path for the commit box and the PR form. Each
    /// target has its own provider/model selection and optional per-repo
    /// custom instructions.
    fn generate_ai(&mut self, target: worker::AiTarget, stage_files: Vec<String>) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(sel) = self.ai_selection(target) else {
            self.toast("No AI model selected. Pick one next to the AI button.", true);
            return;
        };
        let custom = self.repo_prompt(target);
        self.ai_busy = true;

        if sel.provider == "claude" {
            let model = sel.model;
            self.worker.spawn(move || {
                let result = (|| -> Result<ollama::CommitSuggestion, String> {
                    if !stage_files.is_empty() {
                        repo.unstage_all().ok();
                        strerr(repo.stage(&stage_files))?;
                    }
                    let diff = strerr(repo.diff_for_ai())?;
                    let client = claude::Client::from_store(model)
                        .ok_or("Claude is not signed in. Open Settings.")?;
                    strerr(client.commit_message(&diff, custom.as_deref()))
                })();
                Msg::AiSuggestion { target, result }
            });
            return;
        }

        let url = self.effective_ollama_url();
        let model = sel.model;
        self.worker.spawn(move || {
            let result = (|| -> Result<ollama::CommitSuggestion, String> {
                if !stage_files.is_empty() {
                    repo.unstage_all().ok();
                    strerr(repo.stage(&stage_files))?;
                }
                let diff = strerr(repo.diff_for_ai())?;
                strerr(ollama::Client::new(url).commit_message(&model, &diff, custom.as_deref()))
            })();
            Msg::AiSuggestion { target, result }
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
            Msg::Stashes(Ok(stashes)) => self.stashes = stashes,
            Msg::Stashes(Err(e)) => self.toast(e, true),
            Msg::Hunks { file, hunks } => {
                if self.selected_file.as_deref() == Some(file.as_str()) {
                    self.hunks = hunks;
                }
            }
            Msg::CommitFiles { sha, files } => {
                if self.selected_commit.as_deref() == Some(sha.as_str()) {
                    self.commit_file_list = files;
                }
            }
            Msg::GhRepos(result) => {
                self.gh_repos_loading = false;
                match result {
                    Ok(repos) => self.gh_repos = repos,
                    Err(e) => self.toast(e, true),
                }
            }
            Msg::Tags(Ok(tags)) => self.tags = tags,
            Msg::Tags(Err(e)) => self.toast(e, true),
            Msg::ClaudeModels(models) => {
                // Keep the chosen model if still valid, else pick the first.
                if let Some(current) = &self.config.claude_model {
                    if !models.is_empty() && !models.contains(current) {
                        self.config.claude_model = models.first().cloned();
                        self.config.save();
                    }
                }
                self.claude.models = models;
            }

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
                    Ok(prs) => {
                        // Kick off a checks lookup per PR.
                        if let Some(repo) = self.repo.clone() {
                            for pr in &prs {
                                let sha = pr.head_sha.clone();
                                let number = pr.number;
                                let repo = repo.clone();
                                self.worker.spawn(move || {
                                    let summary = github::Client::from_store()
                                        .zip(views::origin_slug(&repo))
                                        .and_then(|(c, slug)| c.checks(&slug, &sha).ok());
                                    match summary {
                                        Some(summary) => Msg::GhPrChecks { number, summary },
                                        None => Msg::Noop,
                                    }
                                });
                            }
                        }
                        self.pr.open_prs = prs;
                    }
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

            Msg::GhBranchChecks { branch, summary } => {
                let current = self.status.as_ref().map(|s| s.branch.as_str());
                if current == Some(branch.as_str()) {
                    self.branch_checks = Some(summary);
                }
            }
            Msg::GhPrChecks { number, summary } => {
                self.pr.checks.insert(number, summary);
            }

            Msg::CiJobDone { index, result } => {
                if let Some(slot) = self.local_ci.results.get_mut(index) {
                    *slot = Some(result);
                }
                if self.local_ci.finished() == self.local_ci.jobs.len() {
                    self.local_ci.running = false;
                    if self.local_ci.all_passed() {
                        self.toast("All local CI checks passed.", false);
                    } else {
                        self.toast("Some local CI checks failed.", true);
                    }
                }
            }

            Msg::Noop => {}

            Msg::OllamaModels(Ok(models)) => {
                if self.config.ollama_model.is_none() {
                    self.config.ollama_model = models.first().map(|m| m.name.clone());
                    self.config.save();
                }
                self.ollama_models = models;
            }
            Msg::OllamaModels(Err(_)) => self.ollama_models.clear(),
            Msg::AiSuggestion { target, result } => {
                self.ai_busy = false;
                match (target, result) {
                    (worker::AiTarget::Commit, Ok(s)) => {
                        self.commit_summary = s.summary;
                        self.commit_description = s.description;
                        self.toast("Commit message generated.", false);
                    }
                    (worker::AiTarget::PullRequest, Ok(s)) => {
                        self.pr.title = s.summary;
                        self.pr.body = s.description;
                        self.toast("PR title and description generated.", false);
                    }
                    (_, Err(e)) => self.toast(e, true),
                }
            }
        }
    }

    pub fn load_conflicts(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || Msg::Conflicts(strerr(repo.conflicts())));
    }

    /// Fetches the Claude model list for the signed-in account.
    pub fn load_claude_models(&mut self) {
        if self.claude.auth_label.is_none() {
            return;
        }
        self.worker.spawn(|| {
            let models = claude::Client::from_store(claude::DEFAULT_MODEL)
                .map(|c| c.models())
                .unwrap_or_default();
            Msg::ClaudeModels(models)
        });
    }

    /// Loads the repo's local CI config into state (jobs + empty results).
    pub fn load_local_ci(&mut self) {
        self.local_ci = Default::default();
        let Some(repo) = self.repo.as_ref() else { return };
        if let Ok(Some(config)) = crate::local_ci::load_config(repo.path()) {
            self.local_ci.results = vec![None; config.jobs.len()];
            self.local_ci.jobs = config.jobs;
        }
    }

    /// Runs all configured local CI jobs on worker threads.
    pub fn run_local_ci(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        if self.local_ci.jobs.is_empty() || self.local_ci.running {
            return;
        }
        self.local_ci.results = vec![None; self.local_ci.jobs.len()];
        self.local_ci.running = true;
        for (index, job) in self.local_ci.jobs.clone().into_iter().enumerate() {
            let root = repo.path().to_path_buf();
            self.worker.spawn(move || Msg::CiJobDone {
                index,
                result: crate::local_ci::run_job(&root, &job),
            });
        }
    }

    /// Reloads the stash list.
    pub fn load_stashes(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || Msg::Stashes(strerr(repo.stash_list())));
    }

    /// Reloads the tag list.
    pub fn load_tags(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || Msg::Tags(strerr(repo.tags())));
    }

    /// Global keyboard shortcuts, using the user-configurable bindings
    /// from Settings. Escape always closes dialogs.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // While rebinding in Settings, keys are captured there instead.
        if self.rebinding.is_some() {
            return;
        }
        let bindings = self.config.shortcuts.clone();
        let (actions, escape) = ctx.input_mut(|i| {
            (bindings.pressed(i), i.key_pressed(egui::Key::Escape))
        });
        for action in actions {
            use shortcuts::Action;
            match action {
                Action::Commit => self.do_commit(),
                Action::Refresh => {
                    self.refresh();
                    self.toast("Refreshed.", false);
                }
                Action::Push => self.shortcut_sync("push"),
                Action::Pull => self.shortcut_sync("pull"),
                Action::RepoPicker => self.dialog = Dialog::RepoPicker,
                Action::ToggleHistory => {
                    self.tab = if self.tab == Tab::Changes { Tab::History } else { Tab::Changes };
                    self.refresh();
                }
            }
        }
        if escape && self.dialog != Dialog::None {
            if self.dialog == Dialog::GitHub {
                self.gh.device = None;
                self.gh.polling = false;
            }
            self.dialog = Dialog::None;
        }
    }

    fn shortcut_sync(&mut self, action: &str) {
        let Some(repo) = self.repo.clone() else { return };
        let token = self.gh_token();
        let set_upstream = !self.status.as_ref().map(|s| s.has_upstream).unwrap_or(false);
        let action = action.to_string();
        self.worker.spawn(move || {
            let auth = token.as_deref();
            let result = if action == "pull" {
                repo.pull(auth).map(|_| "Pulled.".to_string())
            } else {
                repo.push(set_upstream, auth).map(|_| "Pushed.".to_string())
            };
            Msg::Done { message: strerr(result), refresh: true }
        });
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
        self.handle_shortcuts(ctx);

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
        self.auto_fetch();
        ctx.request_repaint_after(Duration::from_secs(3));

        views::toolbar(self, ctx);
        views::sidebar(self, ctx);
        views::diff_panel(self, ctx);
        dialogs::show(self, ctx);
        views::toasts(self, ctx);
    }
}
