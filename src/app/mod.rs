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

pub mod agent_tab;
pub mod dialogs;
pub mod editor;
pub mod graph;
pub mod markdown;
pub mod shortcuts;
pub mod syntax;
pub mod textdiff;
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
use worker::{strerr, AgentKind, LspReply, Msg, Worker};

/// Runs the desktop app. Blocks until the window closes.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("DevDock")
            // Must match the desktop file name (devdock.desktop) so Linux
            // shells associate the window with the right name and icon.
            .with_app_id("devdock")
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "DevDock",
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
    /// Model that drives the conflict-resolution harness. It reads the
    /// repository and proposes edits, so it is usually worth a stronger
    /// model than commit messages get.
    #[serde(default)]
    pub conflict_ai: Option<AiSelection>,
    /// Model that drives the code review gate. `[review] provider/model` in
    /// `.git-manage-ci.toml` overrides this when the repository sets it.
    #[serde(default)]
    pub review_ai: Option<AiSelection>,
    /// Model that drives the coding agent. It reads, edits, and verifies, so
    /// this is the one worth pointing at your strongest model.
    #[serde(default)]
    pub coding_ai: Option<AiSelection>,
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
    /// Appended to the system prompt for AI conflict resolution.
    #[serde(default)]
    pub conflict: String,
    /// Optional Markdown file whose contents are appended for conflicts.
    #[serde(default)]
    pub conflict_file: Option<String>,
}

/// What one agentic run produced, on its way back to the UI thread.
#[derive(Debug, Clone)]
pub struct AgentReport {
    /// The model's closing summary, rendered as Markdown.
    pub summary: String,
    /// Proposed file changes. Nothing has been written.
    pub edits: Vec<crate::agent::PendingEdit>,
    /// Whether a budget cut the run short.
    pub truncated: bool,
}

/// One proposed change plus the user's decision about it.
#[derive(Debug, Clone)]
pub struct ProposedEdit {
    pub edit: crate::agent::PendingEdit,
    /// Whether the user has ticked this change for applying. Off by default:
    /// the model has worktree-wide reach, so every write is a deliberate
    /// choice rather than something to click past.
    pub accepted: bool,
    /// Set once written to disk, so a second Apply cannot double-write.
    pub applied: bool,
    /// Set when the proposal still contains conflict markers, which means
    /// the file is not actually resolved.
    pub unresolved: bool,
}

/// The conflict-resolution harness: what it is doing, and what it wants to
/// change. See [`crate::agent`] for the loop itself.
#[derive(Default)]
pub struct AgentState {
    pub running: bool,
    /// Live progress lines: what the model read, searched, and proposed.
    pub log: Vec<String>,
    /// The model's closing summary, once it finishes.
    pub summary: String,
    pub error: Option<String>,
    /// Proposed changes awaiting the user's decision.
    pub edits: Vec<ProposedEdit>,
    /// Which proposal's diff is open.
    pub selected: Option<usize>,
    pub truncated: bool,
}

impl AgentState {
    /// How many proposals are ticked and not yet written.
    pub fn pending_count(&self) -> usize {
        self.edits.iter().filter(|e| e.accepted && !e.applied).count()
    }
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
        crate::secure_store::config_dir().join("config.json")
    }

    pub fn load() -> Self {
        crate::secure_store::read(&Self::path())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = crate::secure_store::write(&Self::path(), &json);
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
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Tab {
    Changes,
    History,
    Checks,
    /// The code editor, with language server support.
    Editor,
    /// The coding agent.
    Agent,
}

/// Which modal dialog is open, if any.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Dialog {
    None,
    RepoPicker,
    GitHub,
    PullRequests,
    Conflicts,
    Settings,
    /// Ask for a remote URL before the first publish.
    AddRemote,
    /// Uncommitted changes exist; ask how to handle them before switching
    /// to the branch named inside.
    SwitchBranch(String),
    /// Review an AI-drafted local CI config before writing it to disk.
    CiConfigReview,
    /// Full PR review: diffs, inline comments, approve/request changes.
    PrReview,
    /// Confirmation gate for a destructive action.
    Confirm(ConfirmAction),
    /// AI review findings, with the option to act on them or proceed anyway.
    ReviewGate,
    /// Failing local CI checks, with the option to proceed anyway.
    ChecksGate,
    /// Changes the conflict harness proposed, each awaiting confirmation
    /// before anything is written to the worktree.
    AgentChanges,
    /// Name a symbol for a workspace-wide rename.
    Rename,
}

/// A destructive action awaiting user confirmation.
///
/// Every irreversible (or hard-to-reverse) operation routes through this
/// gate so nothing is destroyed on a single misclick.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfirmAction {
    /// Discard working changes to one file (restore/delete).
    DiscardFile(String),
    /// Delete a stash entry without applying it.
    DropStash(u32),
    /// Delete a local branch.
    DeleteBranch(String),
    /// Abort the in-progress merge.
    AbortMerge,
    /// Abort the in-progress rebase.
    AbortRebase,
    /// Undo (soft reset) the last commit.
    UndoCommit(String),
    /// Create an inverse commit for the given sha/subject.
    RevertCommit { sha: String, subject: String },
    /// Discard every uncommitted change in the working tree.
    DiscardAll(usize),
    /// Merge the current branch into `target`. `protected` reflects GitHub
    /// branch rules on the target.
    MergeInto { source: String, target: String, protected: bool },
    /// Regenerate AI text over existing user-visible text (commit message
    /// or PR title/description).
    OverwriteAiText(worker::AiTarget),
}

impl ConfirmAction {
    /// Dialog title.
    pub fn title(&self) -> &'static str {
        match self {
            Self::DiscardFile(_) => "Discard changes?",
            Self::DropStash(_) => "Drop stash?",
            Self::DeleteBranch(_) => "Delete branch?",
            Self::AbortMerge => "Abort merge?",
            Self::AbortRebase => "Abort rebase?",
            Self::UndoCommit(_) => "Undo commit?",
            Self::RevertCommit { .. } => "Revert commit?",
            Self::DiscardAll(_) => "Discard all changes?",
            Self::MergeInto { .. } => "Confirm merge",
            Self::OverwriteAiText(worker::AiTarget::Commit) => "Overwrite commit message?",
            Self::OverwriteAiText(worker::AiTarget::PullRequest) => {
                "Overwrite PR title and description?"
            }
            // Only the text-generating tasks can overwrite a field.
            Self::OverwriteAiText(_) => "Overwrite generated text?",
        }
    }

    /// Explanation of exactly what will happen.
    pub fn body(&self) -> String {
        match self {
            Self::DiscardFile(path) => format!(
                "Your uncommitted changes to \"{path}\" will be permanently lost.\n\
                 Tracked files are restored to the last commit; untracked files are deleted."
            ),
            Self::DropStash(_) => {
                "The stashed changes will be permanently deleted without being applied.".into()
            }
            Self::DeleteBranch(name) => format!(
                "The local branch \"{name}\" will be deleted.\n\
                 Fails safely if it has unmerged commits."
            ),
            Self::AbortMerge => {
                "The merge stops and the branch returns to its state before the merge. \
                 Any conflict resolutions you made are discarded.".into()
            }
            Self::AbortRebase => {
                "The rebase stops and the branch returns to its state before the rebase. \
                 Any conflict resolutions you made are discarded.".into()
            }
            Self::UndoCommit(subject) => format!(
                "\"{subject}\" is removed from history. Its changes stay staged, \
                 so you can edit and re-commit them."
            ),
            Self::RevertCommit { subject, .. } => format!(
                "A new commit will be created that undoes \"{subject}\". \
                 History is preserved; this is safe for pushed commits."
            ),
            Self::MergeInto { protected, .. } => {
                if *protected {
                    "The target is protected on GitHub: the merged result may be \
                     rejected on push. Prefer a pull request."
                        .into()
                } else {
                    "You will end up on the target branch; push to publish.".into()
                }
            }
            Self::DiscardAll(count) => format!(
                "All {count} changed file(s) will be permanently reset.\n\
                 Tracked files return to the last commit; untracked files are deleted.\n\
                 Consider stashing instead if you might want them back."
            ),
            Self::OverwriteAiText(worker::AiTarget::Commit) => {
                "The commit box already has text. Generating replaces it with \
                 the AI's suggestion, and anything you typed is lost."
                    .into()
            }
            Self::OverwriteAiText(worker::AiTarget::PullRequest) => {
                "The PR form already has a title or description. Generating \
                 replaces both with the AI's suggestion, and anything you \
                 typed is lost."
                    .into()
            }
            Self::OverwriteAiText(_) => {
                "The field already has text. Generating replaces it.".into()
            }
        }
    }

    /// Confirm button label (specific beats generic).
    pub fn verb(&self) -> &'static str {
        match self {
            Self::DiscardFile(_) => "Discard changes",
            Self::DropStash(_) => "Drop stash",
            Self::DeleteBranch(_) => "Delete branch",
            Self::AbortMerge => "Abort merge",
            Self::AbortRebase => "Abort rebase",
            Self::UndoCommit(_) => "Undo commit",
            Self::RevertCommit { .. } => "Revert commit",
            Self::DiscardAll(_) => "Discard everything",
            Self::MergeInto { protected, .. } => {
                if *protected { "Merge anyway (may not push)" } else { "Merge" }
            }
            Self::OverwriteAiText(_) => "Overwrite and generate",
        }
    }
}

/// How to handle uncommitted changes during a branch switch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckoutMode {
    /// Clean tree, plain checkout.
    Plain,
    /// Carry the uncommitted changes into the new branch.
    Bring,
    /// Stash first, then switch with a clean tree.
    Stash,
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
    /// Mergeable state keyed by PR number (false = has conflicts).
    pub mergeable: std::collections::HashMap<u64, Option<bool>>,
    /// In-app review session state.
    pub review: PrReviewState,
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
    /// Path currently being resolved by AI, shown as a busy indicator.
    pub ai_busy: Option<String>,
    /// AI proposal awaiting user review: nothing is written to the working
    /// tree until the user explicitly accepts (or edits then saves) it.
    pub ai_proposal: Option<AiMergeProposal>,
}

/// An AI-suggested merge for one file, pending user confirmation.
pub struct AiMergeProposal {
    pub path: String,
    pub content: String,
}

/// State for reviewing one pull request inside the app.
#[derive(Default)]
pub struct PrReviewState {
    /// PR being reviewed; None when the dialog is closed.
    pub pr: Option<github::PullRequest>,
    pub loading: bool,
    pub files: Vec<github::PrFile>,
    /// Reviews already submitted on this PR.
    pub reviews: Vec<github::PrReview>,
    /// Which file's diff is expanded.
    pub selected: Option<usize>,
    /// Pending inline comments (not yet submitted).
    pub pending: Vec<github::ReviewComment>,
    /// Overall review body text.
    pub body: String,
    /// Draft text for a new inline comment: (file index, line, side).
    pub comment_target: Option<(usize, u64, String)>,
    pub comment_draft: String,
    pub submitting: bool,
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
    /// Push integration from the config file.
    pub on_push: crate::local_ci::OnPush,
    /// A push waiting for the current CI run to finish:
    /// (action, set_upstream). Executed when all jobs pass.
    pub pending_push: Option<(String, bool)>,
    /// An action held back because checks failed, kept so the user can look
    /// at the failures and still choose to proceed. Dropped on cancel.
    pub blocked: Option<GatedAction>,
    /// Completed runs, newest first, for the Checks tab.
    pub history: Vec<CiRun>,
    /// When the current run started.
    pub run_started: Option<Instant>,
    /// What triggered the current run.
    pub trigger: CiTrigger,
}

/// An action held back while the AI reviewer runs, and resumed if the user
/// accepts the findings or overrides them.
#[derive(Clone, PartialEq, Eq)]
pub enum GatedAction {
    Push { action: String, set_upstream: bool },
    PullRequest,
}

impl GatedAction {
    /// How the override button describes proceeding anyway.
    pub fn override_label(&self) -> &'static str {
        match self {
            Self::Push { action, .. } if action == "force-push" => "Force-push anyway",
            Self::Push { .. } => "Push anyway",
            Self::PullRequest => "Create pull request anyway",
        }
    }

    pub fn noun(&self) -> &'static str {
        match self {
            Self::Push { .. } => "push",
            Self::PullRequest => "pull request",
        }
    }
}

/// AI code review state for the current repository.
#[derive(Default)]
pub struct ReviewState {
    /// `[review]` settings, re-read from the config file with the CI jobs.
    pub config: crate::review::ReviewConfig,
    pub running: bool,
    /// The most recent outcome, kept so the Checks tab can show it after the
    /// gate dialog is dismissed.
    pub outcome: Option<crate::review::ReviewOutcome>,
    /// Why the last review could not be produced, if it failed. A failed
    /// review never blocks: it is reported and the action proceeds.
    pub error: Option<String>,
    /// The action waiting on this review's verdict.
    pub pending: Option<GatedAction>,
    /// Which finding's detail is expanded in the list.
    pub expanded: Option<usize>,
}

/// What started a CI run.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum CiTrigger {
    #[default]
    Manual,
    Push,
    PullRequest,
}

impl CiTrigger {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Push => "push",
            Self::PullRequest => "pull request",
        }
    }
}

/// One finished CI run for the Checks tab history.
pub struct CiRun {
    pub when: std::time::SystemTime,
    pub trigger: CiTrigger,
    pub results: Vec<crate::local_ci::JobResult>,
    pub passed: bool,
    pub total_secs: f32,
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
    /// CI checks for the default branch (main/master), shown alongside.
    pub main_checks: Option<(String, github::ChecksSummary)>,
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
    /// Whether the hunk bar shows every hunk. Files with many hunks collapse
    /// to [`views::HUNK_BAR_LIMIT`] buttons so the bar cannot crowd out the
    /// diff. Reset whenever the selected file or its hunks change.
    pub hunks_expanded: bool,
    /// Selected changed lines per hunk index (for line-level staging).
    pub line_sel: std::collections::HashSet<(usize, usize)>,
    /// Which diff side is shown for the selected file.
    pub show_staged: bool,
    /// Blame lines when blame view is active.
    pub blame: Option<Vec<crate::git::BlameLine>>,

    // history details
    pub commit_file_list: Vec<crate::git::CommitFileChange>,

    // commit graph (all branches), shown when graph_open
    pub graph: Vec<graph::GraphNode>,
    pub graph_open: bool,

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
    pub review: ReviewState,
    pub conflicts: ConflictState,
    /// The conflict-resolution harness and the changes it proposes.
    pub agent: AgentState,
    /// Open buffers and everything the language server contributes.
    pub editor: editor::EditorState,
    /// The coding agent's tab: task, transcript, and pending changes.
    pub coding: agent_tab::CodingState,
    /// Language servers for the open repository, started on demand.
    pub lsp: std::sync::Arc<crate::lsp::Manager>,
    /// The egui context, kept so background work started from a message
    /// handler can still ask for repaints.
    ctx: egui::Context,
    /// AI CI-config generation: busy flag and the editable proposal text
    /// shown in the review dialog. Nothing is written until confirmed.
    pub ci_ai_busy: bool,
    pub ci_ai_proposal: String,
    pub ollama_url_input: String,
    pub ollama_models: Vec<ollama::Model>,

    // feedback
    pub toast: Option<Toast>,
    pub busy: bool,
    /// Sync operation in flight ("fetch"/"pull"/"push"/"force-push"),
    /// shown as a spinner on the toolbar sync button.
    pub sync_op: Option<&'static str>,
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
            main_checks: None,
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
            ci_ai_busy: false,
            ci_ai_proposal: String::new(),
            diff_title: String::new(),
            diff_text: String::new(),
            hunks: Vec::new(),
            hunks_expanded: false,
            line_sel: Default::default(),
            show_staged: false,
            blame: None,
            commit_file_list: Vec::new(),
            graph: Vec::new(),
            graph_open: false,
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
            review: Default::default(),
            conflicts: Default::default(),
            agent: Default::default(),
            editor: Default::default(),
            coding: Default::default(),
            // Replaced when a repository opens; a manager with no servers
            // running costs nothing until a file needs one.
            lsp: std::sync::Arc::new(crate::lsp::Manager::new(
                std::path::Path::new("."),
                Some(repaint_handle(ctx)),
            )),
            ctx: ctx.clone(),
            ollama_models: Vec::new(),
            toast: None,
            busy: false,
            sync_op: None,
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
        // Quietly check GitHub sign-in and Ollama models. Retry the
        // profile fetch: a transient network failure at startup must not
        // make a valid token look signed-out.
        self.worker.spawn(|| {
            let Some(client) = github::Client::from_store() else {
                return Msg::GhUser(None); // genuinely signed out
            };
            for attempt in 0..3 {
                match client.user() {
                    Ok(user) => return Msg::GhUser(Some(user)),
                    Err(_) if attempt < 2 => {
                        std::thread::sleep(std::time::Duration::from_millis(
                            500 * (attempt + 1),
                        ));
                    }
                    Err(_) => break,
                }
            }
            // Token exists but GitHub is unreachable: stay signed in with
            // a placeholder profile instead of flip-flopping to Sign in.
            Msg::GhUser(Some(github::User {
                login: "(offline)".into(),
                name: None,
                avatar_url: String::new(),
            }))
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
    /// plus the default branch (main) when different, throttled to once
    /// every 30 seconds.
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
        {
            let repo = repo.clone();
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
        // Default branch status (only when we're not already on it).
        let main_branch = self.default_branch();
        if self.status.as_ref().map(|s| s.branch != main_branch).unwrap_or(false) {
            self.worker.spawn(move || {
                let result = (|| -> Option<(String, github::ChecksSummary)> {
                    let client = github::Client::from_store()?;
                    let slug = views::origin_slug(&repo)?;
                    let summary = client.checks(&slug, &main_branch).ok()?;
                    Some((main_branch, summary))
                })();
                match result {
                    Some((branch, summary)) => Msg::GhMainChecks { branch, summary },
                    None => Msg::Noop,
                }
            });
        } else {
            self.main_checks = None;
        }
    }

    /// Best guess at the default branch: a local `main` or `master`.
    fn default_branch(&self) -> String {
        self.branches
            .as_ref()
            .and_then(|b| {
                b.local
                    .iter()
                    .find(|br| br.name == "main" || br.name == "master")
                    .map(|br| br.name.clone())
            })
            .unwrap_or_else(|| "main".into())
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
        self.hunks_expanded = false;
        self.worker.spawn(move || {
            let result = (|| -> Result<String, crate::git::GitError> {
                let mut skipped = Vec::new();
                if !files.is_empty() {
                    repo.unstage_all().ok();
                    skipped = repo.stage(&files)?;
                }
                let sha = repo.commit(&summary, &description, amend)?;
                let verb = if amend { "Amended" } else { "Committed" };
                // Say so rather than letting a file drop out of the commit
                // without a word.
                if skipped.is_empty() {
                    Ok(format!("{verb} {}", &sha[..7]))
                } else {
                    Ok(format!(
                        "{verb} {} — left out {} (ignored by .gitignore and not tracked)",
                        &sha[..7],
                        skipped.join(", ")
                    ))
                }
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
            worker::AiTarget::Conflict => self.config.conflict_ai.clone(),
            worker::AiTarget::Review => self.config.review_ai.clone(),
            worker::AiTarget::Coding => self.config.coding_ai.clone(),
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
            worker::AiTarget::Conflict => self.config.conflict_ai = Some(sel),
            worker::AiTarget::Review => self.config.review_ai = Some(sel),
            worker::AiTarget::Coding => self.config.coding_ai = Some(sel),
        }
        self.config.save();
    }

    /// Entry point for the commit-box AI button: asks for confirmation
    /// first when the box already has text the generation would replace.
    pub fn request_ai_message(&mut self) {
        if !self.commit_summary.trim().is_empty()
            || !self.commit_description.trim().is_empty()
        {
            self.dialog =
                Dialog::Confirm(ConfirmAction::OverwriteAiText(worker::AiTarget::Commit));
            return;
        }
        self.generate_ai_message();
    }

    /// Entry point for the PR-form AI button: asks for confirmation first
    /// when the form already has a title or description.
    pub fn request_pr_text(&mut self) {
        if !self.pr.title.trim().is_empty() || !self.pr.body.trim().is_empty() {
            self.dialog = Dialog::Confirm(ConfirmAction::OverwriteAiText(
                worker::AiTarget::PullRequest,
            ));
            return;
        }
        self.generate_pr_text();
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
            worker::AiTarget::Conflict => (&prompts.conflict, &prompts.conflict_file),
            // Review guidance is per-repository and committed: it comes from
            // `[review] instructions` in .git-manage-ci.toml, not from here.
            worker::AiTarget::Review => return None,
            // The coding agent uses the repository's review instructions,
            // which is where a project already describes how its code should
            // be written; see `coding_instructions`.
            worker::AiTarget::Coding => return None,
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

    /// Custom AI instructions for conflict resolution in the current repo:
    /// inline text plus a linked Markdown file, when configured.
    fn conflict_prompt(&self) -> Option<String> {
        let repo = self.repo.as_ref()?;
        let prompts = self.config.repo_prompts.get(&repo.path().display().to_string())?;
        let mut parts: Vec<String> = Vec::new();
        let inline = prompts.conflict.trim();
        if !inline.is_empty() {
            parts.push(inline.to_string());
        }
        if let Some(path) = &prompts.conflict_file {
            match std::fs::read_to_string(path) {
                Ok(contents) if !contents.trim().is_empty() => {
                    parts.push(contents.trim().to_string());
                }
                _ => {}
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
        // The stored token is the source of truth; gh.user is only the
        // fetched profile and can lag behind (offline start, rate limit).
        github::TokenStore::load()
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
                    self.branch_checks = None;
                    // CI state and history belong to the previous repo.
                    self.local_ci = Default::default();
                    self.load_local_ci();
                    // Graph belongs to the previous repo too.
                    self.graph.clear();
                    self.graph_open = false;
                    // Language servers are per-workspace: stop the previous
                    // repository's and start fresh, and drop its buffers.
                    self.lsp.shutdown_all();
                    self.lsp = std::sync::Arc::new(crate::lsp::Manager::new(
                        std::path::Path::new(&path),
                        Some(repaint_handle(&self.ctx)),
                    ));
                    self.editor = Default::default();
                    self.refresh();
                }
                Err(e) => self.toast(e.to_string(), true),
            },
            Msg::RepoOpened(Err(e)) => self.toast(e, true),
            Msg::Status(Ok(status)) => {
                // If the file shown in the diff viewport no longer has
                // changes (discarded, stashed, committed), clear the view.
                if let Some(path) = self.selected_file.clone() {
                    if !status.files.iter().any(|f| f.path == path) {
                        views::clear_diff_view(self);
                    }
                }
                // Branch switch: stale per-branch state must reset.
                let branch_changed = self
                    .status
                    .as_ref()
                    .map(|old| old.branch != status.branch)
                    .unwrap_or(false);
                if branch_changed {
                    self.branch_checks = None;
                    self.last_checks_refresh = None;
                    views::clear_diff_view(self);
                    self.unchecked.clear();
                    // Reload CI config from the new branch's worktree state.
                    self.load_local_ci();
                    // Graph shows all branches but HEAD markers move.
                    if self.graph_open {
                        self.load_graph();
                    }
                }
                self.status = Some(status);
            }
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
                self.pr.review.submitting = false;
                self.busy = false;
                self.sync_op = None;
                match message {
                    Ok(m) => {
                        if !m.is_empty() {
                            self.toast(m, false);
                        }
                    }
                    Err(e) => {
                        // Conflicts from stash apply / checkout land here:
                        // open the resolver instead of only toasting.
                        if e.to_lowercase().contains("conflict") {
                            self.toast(
                                "Conflicts detected. Opening the resolver…",
                                true,
                            );
                            self.load_conflicts();
                        } else {
                            self.toast(e, true);
                        }
                    }
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
                    self.hunks_expanded = false;
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
            Msg::GhUser(user) => {
                if user.is_some() || github::TokenStore::load().is_none() {
                    self.gh.user = user;
                }
            }
            Msg::GhPrs(result) => {
                self.pr.loading = false;
                match result {
                    Ok(prs) => {
                        // Kick off checks + mergeable lookups per PR.
                        if let Some(repo) = self.repo.clone() {
                            for pr in &prs {
                                let sha = pr.head_sha.clone();
                                let number = pr.number;
                                {
                                    let repo = repo.clone();
                                    self.worker.spawn(move || {
                                        let summary = github::Client::from_store()
                                            .zip(views::origin_slug(&repo))
                                            .and_then(|(c, slug)| c.checks(&slug, &sha).ok());
                                        match summary {
                                            Some(summary) => {
                                                Msg::GhPrChecks { number, summary }
                                            }
                                            None => Msg::Noop,
                                        }
                                    });
                                }
                                let repo = repo.clone();
                                self.worker.spawn(move || {
                                    let mergeable = github::Client::from_store()
                                        .zip(views::origin_slug(&repo))
                                        .and_then(|(c, slug)| {
                                            c.pr_mergeable(&slug, number).ok()
                                        });
                                    match mergeable {
                                        Some(mergeable) => {
                                            Msg::GhPrMergeable { number, mergeable }
                                        }
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
            Msg::GhMainChecks { branch, summary } => {
                self.main_checks = Some((branch, summary));
            }
            Msg::Graph(nodes) => self.graph = nodes,
            Msg::MergePrompt { source, target, protected } => {
                self.busy = false;
                self.confirm(ConfirmAction::MergeInto { source, target, protected });
            }
            Msg::GhPrChecks { number, summary } => {
                self.pr.checks.insert(number, summary);
            }
            Msg::GhPrMergeable { number, mergeable } => {
                self.pr.mergeable.insert(number, mergeable);
            }
            Msg::GhPrReviewData { number, files, reviews } => {
                if self.pr.review.pr.as_ref().map(|p| p.number) == Some(number) {
                    self.pr.review.loading = false;
                    self.pr.review.files = files;
                    self.pr.review.reviews = reviews;
                }
            }

            Msg::ReviewDone(result) => {
                self.review.running = false;
                match result {
                    Ok(outcome) => {
                        // `should_block` covers both output styles: the
                        // fail_on threshold for findings, the reviewer's
                        // verdict line for custom Markdown.
                        let blocks = outcome.should_block(&self.review.config);
                        let (high, medium, low) = outcome.tally();
                        let markdown = outcome.markdown.is_some();
                        self.review.outcome = Some(outcome);
                        // The gate dialog only makes sense when an action is
                        // actually being held; a manual review just reports.
                        if blocks && self.review.pending.is_some() {
                            // Hold the action and put the findings and the
                            // reviewer's reasoning in front of the user.
                            self.dialog = Dialog::ReviewGate;
                        } else {
                            let note = if markdown {
                                "Review ready — see the Checks tab.".to_string()
                            } else if high + medium + low == 0 {
                                "Review found nothing.".to_string()
                            } else {
                                format!(
                                    "Review: {high} high, {medium} medium, {low} low. \
                                     Not blocking; see the Checks tab."
                                )
                            };
                            self.toast(note, false);
                            if let Some(gated) = self.review.pending.take() {
                                self.perform(gated);
                            }
                        }
                    }
                    Err(e) => {
                        // Never block on our own failure — report and proceed.
                        self.review.error = Some(e.clone());
                        self.toast(format!("AI review did not run: {e}. Proceeding."), true);
                        if let Some(gated) = self.review.pending.take() {
                            self.perform(gated);
                        }
                    }
                }
            }
            Msg::CiJobDone { index, result } => {
                if let Some(slot) = self.local_ci.results.get_mut(index) {
                    *slot = Some(result);
                }
                if self.local_ci.finished() == self.local_ci.jobs.len() {
                    self.local_ci.running = false;
                    let passed = self.local_ci.all_passed();
                    // Record the run in the Checks tab history.
                    let results: Vec<crate::local_ci::JobResult> =
                        self.local_ci.results.iter().flatten().cloned().collect();
                    let total_secs = self
                        .local_ci
                        .run_started
                        .take()
                        .map(|t| t.elapsed().as_secs_f32())
                        .unwrap_or_else(|| results.iter().map(|r| r.duration_secs).sum());
                    self.local_ci.history.insert(
                        0,
                        CiRun {
                            when: std::time::SystemTime::now(),
                            trigger: self.local_ci.trigger,
                            results,
                            passed,
                            total_secs,
                        },
                    );
                    self.local_ci.history.truncate(50);
                    // A push may be waiting on this run.
                    if let Some((action, set_upstream)) = self.local_ci.pending_push.take() {
                        if passed {
                            // Jobs cleared; the AI reviewer is the next gate.
                            self.toast("Checks passed.", false);
                            self.gate_with_review(GatedAction::Push { action, set_upstream });
                        } else if self.local_ci.on_push.block_on_failure {
                            // Hold the push and show what failed, rather than
                            // discarding it. The checks are the developer's own
                            // and can be wrong or irrelevant to this change, so
                            // the gate offers a way through — the same bargain
                            // the AI reviewer makes.
                            self.tab = Tab::Checks;
                            self.local_ci.expanded = self
                                .local_ci
                                .results
                                .iter()
                                .position(|r| r.as_ref().map(|x| !x.ok).unwrap_or(false));
                            self.local_ci.blocked =
                                Some(GatedAction::Push { action, set_upstream });
                            self.dialog = Dialog::ChecksGate;
                        } else {
                            self.toast("Checks failed (non-blocking). Pushing anyway…", true);
                            self.execute_push(&action, set_upstream);
                        }
                    } else if passed {
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
            Msg::AiCiConfig { result } => {
                self.ci_ai_busy = false;
                match result {
                    Ok(toml_text) => {
                        // Proposal only: opens for review, never auto-written.
                        self.ci_ai_proposal = toml_text;
                        self.dialog = Dialog::CiConfigReview;
                    }
                    Err(e) => self.toast(e, true),
                }
            }
            Msg::AiMergeProposal { path, result } => {
                self.conflicts.ai_busy = None;
                match result {
                    Ok(content) => {
                        // Proposal only: shown for review, never auto-applied.
                        self.conflicts.editor = content.clone();
                        self.conflicts.ai_proposal =
                            Some(AiMergeProposal { path: path.clone(), content });
                        self.toast(
                            format!("AI proposed a merge for {path}. Review before accepting."),
                            false,
                        );
                    }
                    Err(e) => self.toast(e, true),
                }
            }
            Msg::TrackedFiles(files) => {
                self.editor.quick_open.loading = false;
                self.editor.quick_open.files = files;
            }
            Msg::Lsp(reply) => self.handle_lsp(reply),
            Msg::AgentEvent { kind, line } => match kind {
                AgentKind::Conflict => self.agent.log.push(line),
                AgentKind::Coding => self.coding.log.push(line),
            },
            Msg::AgentDone { kind: AgentKind::Coding, result } => {
                self.finish_coding_run(result)
            }
            Msg::AgentDone { kind: _, result } => {
                self.agent.running = false;
                match result {
                    Ok(report) => {
                        let conflicted: Vec<String> =
                            self.conflicts.files.iter().map(|f| f.path.clone()).collect();
                        self.agent.summary = report.summary;
                        self.agent.truncated = report.truncated;
                        self.agent.edits = report
                            .edits
                            .into_iter()
                            .map(|edit| {
                                // A "resolution" that still has markers in it
                                // is flagged, not silently offered as done.
                                let unresolved = conflicted.contains(&edit.path)
                                    && crate::agent::conflict::has_conflict_markers(
                                        &edit.after,
                                    );
                                ProposedEdit {
                                    edit,
                                    accepted: false,
                                    applied: false,
                                    unresolved,
                                }
                            })
                            .collect();
                        self.agent.selected = (!self.agent.edits.is_empty()).then_some(0);
                        if self.agent.edits.is_empty() {
                            self.toast(
                                "The AI proposed no changes. Its notes are in the \
                                 conflict resolver.",
                                true,
                            );
                        } else {
                            self.dialog = Dialog::AgentChanges;
                        }
                    }
                    Err(e) => {
                        self.agent.error = Some(e.clone());
                        self.toast(e, true);
                    }
                }
            }
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
                    // The harness tasks report through Msg::AgentDone.
                    (
                        worker::AiTarget::Conflict
                        | worker::AiTarget::Review
                        | worker::AiTarget::Coding,
                        Ok(_),
                    ) => {}
                    (_, Err(e)) => self.toast(e, true),
                }
            }
        }
    }

    /// Applies one language server answer to the editor.
    fn handle_lsp(&mut self, reply: LspReply) {
        use crate::lsp::protocol;
        self.editor.busy = self.editor.busy.saturating_sub(1);
        match reply {
            LspReply::Opened { path, server, error } => match error {
                Some(e) => {
                    // Not having a language server is a normal state, not a
                    // failure: the editor still edits.
                    self.editor.error = Some(e);
                    if let Some(file) = self.editor.file_mut(&path) {
                        file.synced = false;
                    }
                }
                None => {
                    self.editor.error = None;
                    self.editor.status = Some(server);
                }
            },
            LspReply::Hover { path, text } => {
                self.editor.hover.requesting = false;
                // Ignore an answer for a file that is no longer in front.
                if self.editor.active_file().is_some_and(|f| f.path == path) {
                    self.editor.hover.text = text;
                }
            }
            LspReply::Definition(locations) => {
                let Some(location) = locations.first() else {
                    self.toast("No definition found.", true);
                    return;
                };
                let Some(path) = protocol::uri_to_path(&location.uri) else { return };
                self.editor_open(&path, Some(location.range.start.line));
            }
            LspReply::References(locations) => {
                if locations.is_empty() {
                    self.toast("No references found.", true);
                }
                self.editor.references = locations;
                self.editor.bottom = editor::BottomPanel::References;
            }
            LspReply::Symbols { path, symbols } => {
                if let Some(file) = self.editor.file_mut(&path) {
                    file.symbols = symbols;
                }
            }
            LspReply::Completion { path, items, anchor } => {
                self.editor.completion.requesting = false;
                if !self.editor.active_file().is_some_and(|f| f.path == path) {
                    return;
                }
                if items.is_empty() {
                    self.editor.completion.close();
                    return;
                }
                self.editor.completion.open = true;
                self.editor.completion.items = items;
                self.editor.completion.anchor = anchor;
                self.editor.completion.selected = 0;
            }
            LspReply::Formatted { path, text, save } => {
                self.editor_replace_text(&path, text);
                if save {
                    self.editor_write_active();
                }
            }
            LspReply::Renamed { new_name, edits } => {
                if edits.is_empty() {
                    self.toast("The language server proposed no changes.", true);
                    return;
                }
                self.stage_rename_proposal(&new_name, edits);
            }
            LspReply::Failed(e) => {
                self.editor.completion.requesting = false;
                self.editor.hover.requesting = false;
                self.toast(e, true);
            }
        }
    }

    /// Turns a rename's edits into proposals in the same review dialog the
    /// AI harness uses.
    ///
    /// A rename touches files that are not open and may not even be in the
    /// current diff, so it goes through confirmation like any other
    /// multi-file change rather than rewriting the worktree behind the
    /// user's back.
    fn stage_rename_proposal(
        &mut self,
        new_name: &str,
        edits: Vec<(std::path::PathBuf, Vec<crate::lsp::protocol::TextEdit>)>,
    ) {
        use crate::lsp::protocol;
        let mut proposals = Vec::new();
        let mut failed = Vec::new();
        for (path, file_edits) in edits {
            // Buffers are keyed by canonical path, and a server's URI may
            // not be canonical (/var vs /private/var on macOS).
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            // Prefer the open buffer's text: it may differ from disk, and
            // that is the text the server just computed offsets against.
            let before = match self.editor.files.iter().find(|f| f.path == path) {
                Some(file) => file.text.clone(),
                None => match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) => {
                        failed.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                },
            };
            let after = protocol::apply_edits(&before, &file_edits);
            if after == before {
                continue;
            }
            let rel = self
                .repo
                .as_ref()
                .and_then(|r| path.strip_prefix(r.path()).ok())
                .unwrap_or(&path)
                .display()
                .to_string();
            proposals.push(ProposedEdit {
                edit: crate::agent::PendingEdit { path: rel, before: Some(before), after },
                accepted: false,
                applied: false,
                unresolved: false,
            });
        }

        if proposals.is_empty() {
            self.toast("Nothing to rename.", true);
            return;
        }
        let count = proposals.len();
        self.agent = AgentState {
            summary: format!(
                "Language server rename to `{new_name}` across {count} file(s).{}",
                if failed.is_empty() {
                    String::new()
                } else {
                    format!("\n\nCould not read: {}", failed.join(", "))
                }
            ),
            edits: proposals,
            selected: Some(0),
            ..Default::default()
        };
        self.dialog = Dialog::AgentChanges;
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

    /// Loads the repo's local CI config into state (jobs + empty results),
    /// preserving the run history.
    pub fn load_local_ci(&mut self) {
        let history = std::mem::take(&mut self.local_ci.history);
        self.local_ci = Default::default();
        self.local_ci.history = history;
        let Some(repo) = self.repo.as_ref() else { return };
        // Picks up per-directory configs too, so a monorepo's packages each
        // contribute their own jobs.
        let Ok(loaded) = crate::local_ci::discover_configs(repo.path()) else { return };
        let nested = loaded.sources.iter().filter(|d| !d.is_empty()).count();
        let ignored = loaded.ignored_gates.clone();
        self.local_ci.results = vec![None; loaded.config.jobs.len()];
        self.local_ci.jobs = loaded.config.jobs;
        self.local_ci.on_push = loaded.config.on_push;
        // Keep the last outcome visible across a config reload; only the
        // settings are re-read.
        self.review.config = loaded.config.review;

        if nested > 0 {
            self.toast(
                format!(
                    "Loaded {} check(s) from {} config file(s).",
                    self.local_ci.jobs.len(),
                    nested + 1
                ),
                false,
            );
        }
        // Ignoring part of someone's config silently would be worse than the
        // limitation itself.
        if !ignored.is_empty() {
            self.toast(
                format!(
                    "[on_push]/[review] in {} apply to the whole repository and \
                     were ignored — set them in the root {}.",
                    ignored.join(", "),
                    crate::local_ci::CONFIG_FILE
                ),
                true,
            );
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
        self.local_ci.run_started = Some(Instant::now());
        for (index, job) in self.local_ci.jobs.clone().into_iter().enumerate() {
            let root = repo.path().to_path_buf();
            self.worker.spawn(move || Msg::CiJobDone {
                index,
                result: crate::local_ci::run_job(&root, &job),
            });
        }
    }

    /// Pushes, honoring the repo's `on_push` local CI config: when enabled,
    /// checks run first and the push executes only if they pass (or
    /// unconditionally when `block_on_failure = false`).
    pub fn push_with_ci(&mut self, action: &str, set_upstream: bool) {
        // Re-read the config so edits apply without reopening dialogs
        // (but never clobber a run already in flight).
        if !self.local_ci.running {
            self.load_local_ci();
        }
        let ci = &self.local_ci;
        if ci.on_push.run && !ci.jobs.is_empty() && !ci.running {
            self.local_ci.pending_push = Some((action.to_string(), set_upstream));
            self.local_ci.trigger = CiTrigger::Push;
            self.run_local_ci();
            // Show progress where the user can see it.
            self.tab = Tab::Checks;
            self.toast(
                format!(
                    "Running {} check(s) before push. Watch the Checks tab.",
                    self.local_ci.jobs.len()
                ),
                false,
            );
            return;
        }
        // No job gate (or none configured): the reviewer is the only gate.
        self.gate_with_review(GatedAction::Push {
            action: action.to_string(),
            set_upstream,
        });
    }

    /// Runs `action` through the AI review gate when `[review] run = true`,
    /// otherwise performs it immediately.
    ///
    /// A review that cannot run at all (no provider signed in, request
    /// failed) reports the reason and lets the action through. A gate that
    /// silently blocks work when its own dependency is missing is worse than
    /// no gate: the first time it happens the feature gets switched off.
    pub fn gate_with_review(&mut self, gated: GatedAction) {
        self.start_review(Some(gated), false);
    }

    /// Shared body of the gated and manual review paths. `force` runs the
    /// review even when `[review] run = false`; `gated` is the action to
    /// resume afterwards, or `None` for a review that gates nothing.
    fn start_review(&mut self, gated: Option<GatedAction>, force: bool) {
        let Some(repo) = self.repo.clone() else { return };
        // Each trigger is enabled independently, falling back to `run` when
        // the repo only set the simple switch.
        let enabled = match &gated {
            Some(GatedAction::Push { .. }) => self.review.config.runs_on_push(),
            Some(GatedAction::PullRequest) => self.review.config.runs_on_pull_request(),
            // A manual review from the Checks tab is its own consent.
            None => true,
        };
        if (!enabled && !force) || self.review.running {
            if let Some(gated) = gated {
                self.perform(gated);
            }
            return;
        }
        let Some((provider, model)) = self.review_provider() else {
            self.toast(
                "AI review is enabled but no model is available. Pick one in \
                 Settings, or set provider/model under [review]. Proceeding.",
                true,
            );
            if let Some(gated) = gated {
                self.perform(gated);
            }
            return;
        };

        // A pull request is reviewed against its target branch; a push
        // against whatever the branch would publish.
        let base = match &gated {
            Some(GatedAction::PullRequest) => Some(self.pr.base.clone()),
            _ => None,
        };
        let cfg = self.review.config.clone();
        let url = self.effective_ollama_url();

        self.review.running = true;
        self.review.error = None;
        self.review.outcome = None;
        self.review.expanded = None;
        self.review.pending = gated;
        self.toast("Reviewing the outgoing diff…", false);

        self.worker.spawn(move || {
            let result = (|| -> Result<crate::review::ReviewOutcome, String> {
                // Read instructions files now rather than at config load, so
                // editing the guidance takes effect on the next review without
                // reloading the config.
                let cfg = cfg.resolve_files(repo.path())?;
                let diff = strerr(repo.diff_for_review(base.as_deref()))?;
                if diff.trim().is_empty() {
                    return Err("Nothing to review: no outgoing changes found.".into());
                }
                let sel = AiSelection { provider, model };
                if !cfg.repo_context {
                    return review_single_shot(&sel, &url, &diff, &cfg);
                }
                match review_with_repo_context(&repo, &sel, &url, &diff, &cfg) {
                    Err(e) if lacks_tool_support(&e) => {
                        // The model cannot call tools, so it cannot read the
                        // repository. Review the diff alone rather than
                        // failing: a diff-only review is the old behaviour,
                        // and a gate that errors out gets switched off.
                        let mut outcome = review_single_shot(&sel, &url, &diff, &cfg)?;
                        outcome.context_log.push(format!("! {e}"));
                        outcome
                            .context_log
                            .push("! reviewed the diff alone, without repository context".into());
                        Ok(outcome)
                    }
                    other => other,
                }
            })();
            Msg::ReviewDone(result)
        });
    }

    /// Reviews the outgoing diff without gating anything, for the Checks
    /// tab's own button. Runs even when `[review] run = false`, since asking
    /// for a review explicitly is its own consent.
    pub fn review_now(&mut self) {
        self.start_review(None, true);
    }

    /// Which provider and model the reviewer should use: the `[review]`
    /// overrides when set, else whatever the app has selected for AI work.
    fn review_provider(&self) -> Option<(String, String)> {
        let cfg = &self.review.config;
        if let (Some(provider), Some(model)) = (&cfg.provider, &cfg.model) {
            return Some((provider.clone(), model.clone()));
        }
        let sel = self.ai_selection(worker::AiTarget::Commit)?;
        Some((
            cfg.provider.clone().unwrap_or(sel.provider),
            cfg.model.clone().unwrap_or(sel.model),
        ))
    }

    /// Carries out an action that has cleared (or been let through) the gate.
    pub fn perform(&mut self, gated: GatedAction) {
        match gated {
            GatedAction::Push { action, set_upstream } => {
                self.execute_push(&action, set_upstream)
            }
            GatedAction::PullRequest => dialogs::create_pr(self),
        }
    }

    /// Runs the actual push/force-push on a worker thread.
    fn execute_push(&mut self, action: &str, set_upstream: bool) {
        let Some(repo) = self.repo.clone() else { return };
        let token = self.gh_token();
        let force = action == "force-push";
        self.busy = true;
        self.sync_op = Some(if force { "force-push" } else { "push" });
        self.worker.spawn(move || {
            let auth = token.as_deref();
            let result = if force {
                repo.force_push(auth).map(|_| "Force-pushed (with lease).".to_string())
            } else {
                repo.push(set_upstream, auth).map(|_| "Pushed.".to_string()).map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("protected branch") || msg.contains("GH006") {
                        crate::git::GitError::Command(format!(
                            "{msg}\n\nGitHub rejected the push: this branch is protected \
                             by repository rules. Open a pull request instead \
                             (Pull Request button in the toolbar)."
                        ))
                    } else if msg.contains("rejected") || msg.contains("non-fast-forward") {
                        crate::git::GitError::Command(format!(
                            "{msg}\n\nHint: after amend/rebase use Force push \
                             (right-click the sync button)."
                        ))
                    } else if msg.contains("Permission denied (publickey") {
                        crate::git::GitError::Command(format!(
                            "{msg}\n\nHint: this remote uses SSH. Add your key to \
                             ssh-agent or switch the remote to HTTPS and sign in \
                             to GitHub in this app."
                        ))
                    } else {
                        e
                    }
                })
            };
            Msg::Done { message: strerr(result), refresh: true }
        });
    }

    /// Opens the in-app review screen for a PR and loads its files and
    /// existing reviews in the background.
    pub fn open_pr_review(&mut self, pr: github::PullRequest) {
        let number = pr.number;
        self.pr.review = PrReviewState {
            pr: Some(pr),
            loading: true,
            ..Default::default()
        };
        self.dialog = Dialog::PrReview;
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || {
            let data = github::Client::from_store()
                .zip(views::origin_slug(&repo))
                .map(|(client, slug)| {
                    let files = client.pr_files(&slug, number).unwrap_or_default();
                    let reviews = client.pr_reviews(&slug, number).unwrap_or_default();
                    (files, reviews)
                });
            match data {
                Some((files, reviews)) => Msg::GhPrReviewData { number, files, reviews },
                None => Msg::Done {
                    message: Err("Not signed in to GitHub".into()),
                    refresh: false,
                },
            }
        });
    }

    /// Submits the current review with the given event
    /// (APPROVE / REQUEST_CHANGES / COMMENT) including pending inline
    /// comments. Called from the review dialog's confirm popup.
    pub fn submit_pr_review(&mut self, event: String) {
        let Some(pr) = self.pr.review.pr.clone() else { return };
        let Some(repo) = self.repo.clone() else { return };
        let body = self.pr.review.body.clone();
        let comments = self.pr.review.pending.clone();
        self.pr.review.submitting = true;
        let number = pr.number;
        self.worker.spawn(move || {
            let result = (|| -> Result<String, String> {
                let client = github::Client::from_store().ok_or("Not signed in")?;
                let slug = views::origin_slug(&repo).ok_or("No github.com remote")?;
                client
                    .submit_review(&slug, number, &event, &body, &comments)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Review submitted on #{number}"))
            })();
            Msg::Done { message: result, refresh: true }
        });
    }

    /// Asks the selected AI to draft a `.git-manage-ci.toml` for this repo
    /// from a scan of its files and manifests. The result opens in a review
    /// dialog; nothing is written until the user confirms.
    pub fn generate_ci_config(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(sel) = self.ai_selection(worker::AiTarget::Commit) else {
            self.toast("No AI model selected. Pick one next to the AI button.", true);
            return;
        };
        self.ci_ai_busy = true;
        let ollama_url = self.effective_ollama_url();
        self.worker.spawn(move || {
            let scan = crate::local_ci::repo_scan(repo.path());
            let result = if sel.provider == "claude" {
                claude::Client::from_store(sel.model)
                    .ok_or("Claude is not signed in. Open Settings.".to_string())
                    .and_then(|c| strerr(c.generate_ci_config(&scan)))
            } else {
                strerr(ollama::Client::new(ollama_url).generate_ci_config(&sel.model, &scan))
            };
            Msg::AiCiConfig { result }
        });
    }

    /// Asks the selected AI to propose a merge for one conflicted file.
    /// The result is only a proposal: it is loaded into the review editor
    /// and must be explicitly confirmed by the user before anything is
    /// written to the working tree or index.
    pub fn ai_resolve_conflict(&mut self, path: String) {
        let Some(file) = self.conflicts.files.iter().find(|f| f.path == path).cloned()
        else {
            return;
        };
        let Some(sel) = self.ai_selection(worker::AiTarget::Conflict) else {
            self.toast("No AI model selected. Pick one next to the AI button.", true);
            return;
        };
        let base = file.base.clone().unwrap_or_default();
        let ours = file.ours.clone().unwrap_or_default();
        let theirs = file.theirs.clone().unwrap_or_default();
        let custom = self.conflict_prompt();
        self.conflicts.ai_busy = Some(path.clone());
        let ollama_url = self.effective_ollama_url();
        self.worker.spawn(move || {
            let custom = custom.as_deref();
            let result = if sel.provider == "claude" {
                claude::Client::from_store(sel.model)
                    .ok_or("Claude is not signed in. Open Settings.".to_string())
                    .and_then(|c| {
                        strerr(c.resolve_conflict(&path, &base, &ours, &theirs, custom))
                    })
            } else {
                strerr(ollama::Client::new(ollama_url).resolve_conflict(
                    &sel.model, &path, &base, &ours, &theirs, custom,
                ))
            };
            Msg::AiMergeProposal { path, result }
        });
    }

    // -- editor -------------------------------------------------------------

    /// Opens `path` in the editor, or focuses it if already open, and
    /// optionally reveals a line.
    ///
    /// A file already open is never re-read from disk: doing so would throw
    /// away unsaved edits every time a diagnostic or a search result pointed
    /// at it.
    pub fn editor_open(&mut self, path: &std::path::Path, reveal: Option<u32>) {
        self.tab = Tab::Editor;
        // Canonicalize first: the same file reached through a symlink or a
        // relative path would otherwise open as a second buffer, and the two
        // would overwrite each other on save.
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path = canonical.as_path();
        if let Some(index) = self.editor.index_of(path) {
            self.editor.active = Some(index);
            if let Some(file) = self.editor.files.get_mut(index) {
                file.reveal = reveal;
            }
            return;
        }

        let text = match std::fs::read(path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.toast(format!("{} is not a text file.", path.display()), true);
                    return;
                }
            },
            Err(e) => {
                self.toast(format!("{}: {e}", path.display()), true);
                return;
            }
        };

        let rel = self
            .repo
            .as_ref()
            .and_then(|r| path.strip_prefix(r.path()).ok())
            .unwrap_or(path)
            .display()
            .to_string();
        let read_only = text.len() > editor::MAX_EDITABLE_BYTES;
        if read_only {
            self.toast(
                format!("{rel} is large; opened read-only.",),
                false,
            );
        }
        self.editor.files.push(editor::OpenFile {
            path: path.to_path_buf(),
            rel,
            lang: syntax::Lang::from_path(&path.display().to_string()),
            saved: text.clone(),
            text,
            symbols: Vec::new(),
            cursor: 0,
            reveal,
            dirty_since: None,
            synced: false,
            read_only,
        });
        self.editor.active = Some(self.editor.files.len() - 1);
        self.editor.completion.close();
        self.lsp_open(path);
    }

    /// Opens the file finder, loading the tracked file list the first time.
    pub fn editor_quick_open(&mut self) {
        self.tab = Tab::Editor;
        self.editor.quick_open.open = true;
        self.editor.quick_open.selected = 0;
        if !self.editor.quick_open.files.is_empty() || self.editor.quick_open.loading {
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        self.editor.quick_open.loading = true;
        self.worker
            .spawn(move || Msg::TrackedFiles(repo.tracked_files().unwrap_or_default()));
    }

    /// Closes a buffer, warning rather than discarding unsaved work.
    pub fn editor_close(&mut self, index: usize) {
        let Some(file) = self.editor.files.get(index) else { return };
        if file.is_dirty() {
            self.toast(
                format!("{} has unsaved changes. Save it first (Ctrl+S).", file.rel),
                true,
            );
            return;
        }
        let path = file.path.clone();
        self.editor.files.remove(index);
        self.editor.active = match self.editor.active {
            Some(active) if active == index => {
                (!self.editor.files.is_empty()).then(|| index.min(self.editor.files.len() - 1))
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        self.editor.completion.close();

        let lsp = self.lsp.clone();
        self.worker.spawn(move || {
            if let Some(client) = lsp.running_for(&path) {
                let _ = client.did_close(&path);
            }
            Msg::Noop
        });
    }

    /// Writes the active buffer to disk.
    ///
    /// With format-on-save enabled this formats first and saves the result,
    /// so what lands on disk is what the language server would produce.
    pub fn editor_save(&mut self) {
        let Some(file) = self.editor.active_file() else { return };
        if self.editor.format_on_save {
            let path = file.path.clone();
            let text = file.text.clone();
            self.lsp_format(&path, &text, true);
            return;
        }
        self.editor_write_active();
    }

    /// The write half of saving, after any formatting.
    fn editor_write_active(&mut self) {
        let Some(file) = self.editor.active_file() else { return };
        let (path, text, rel) = (file.path.clone(), file.text.clone(), file.rel.clone());
        if let Err(e) = std::fs::write(&path, &text) {
            self.toast(format!("{rel}: {e}"), true);
            return;
        }
        if let Some(file) = self.editor.file_mut(&path) {
            file.saved = text.clone();
            file.dirty_since = None;
        }
        self.toast(format!("Saved {rel}"), false);
        // The file changed on disk, so the rest of the app should notice.
        self.refresh();

        let lsp = self.lsp.clone();
        let progress = self.worker.progress();
        self.worker.spawn(move || {
            if let Some(client) = lsp.running_for(&path) {
                let _ = client.did_change(&path, &text);
                let _ = client.did_save(&path, &text);
                // Symbols move around on save; refresh the outline with them.
                if let Ok(symbols) = client.document_symbols(&path) {
                    progress.send(Msg::Lsp(LspReply::Symbols { path, symbols }));
                }
            }
            Msg::Noop
        });
    }

    /// Formats the active buffer through the language server.
    pub fn editor_format(&mut self) {
        let Some(file) = self.editor.active_file() else { return };
        let (path, text) = (file.path.clone(), file.text.clone());
        self.lsp_format(&path, &text, false);
    }

    /// Replaces a buffer's text, keeping the caret from jumping to the top.
    fn editor_replace_text(&mut self, path: &std::path::Path, text: String) {
        let Some(file) = self.editor.file_mut(path) else { return };
        if file.text == text {
            return;
        }
        file.cursor = file.cursor.min(text.len());
        file.text = text;
        file.dirty_since = Some(std::time::Instant::now());
    }

    // -- language server ------------------------------------------------------

    /// Hands a file to its language server, starting the server if needed.
    pub fn lsp_open(&mut self, path: &std::path::Path) {
        let Some(file) = self.editor.file_mut(path) else { return };
        if file.synced {
            return;
        }
        file.synced = true;
        let (path, text) = (file.path.clone(), file.text.clone());
        let lsp = self.lsp.clone();
        let progress = self.worker.progress();
        self.editor.busy += 1;
        self.worker.spawn(move || {
            let reply = match lsp.ensure_for(&path) {
                Ok(client) => {
                    let error = client.did_open(&path, &text).err();
                    if error.is_none() {
                        if let Ok(symbols) = client.document_symbols(&path) {
                            progress.send(Msg::Lsp(LspReply::Symbols {
                                path: path.clone(),
                                symbols,
                            }));
                        }
                    }
                    LspReply::Opened {
                        path,
                        server: client.spec().name.clone(),
                        error,
                    }
                }
                Err(e) => LspReply::Opened { path, server: String::new(), error: Some(e) },
            };
            Msg::Lsp(reply)
        });
    }

    /// Sends the buffer's current text to the server.
    pub fn lsp_sync(&mut self, path: &std::path::Path) {
        let Some(file) = self.editor.file_mut(path) else { return };
        let (path, text) = (file.path.clone(), file.text.clone());
        let lsp = self.lsp.clone();
        self.worker.spawn(move || {
            if let Some(client) = lsp.running_for(&path) {
                let _ = client.did_change(&path, &text);
            }
            Msg::Noop
        });
    }

    pub fn lsp_hover(&mut self, path: &std::path::Path, position: crate::lsp::protocol::Position) {
        let path = path.to_path_buf();
        let lsp = self.lsp.clone();
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Hover { path, text: None });
            };
            match client.hover(&path, position) {
                Ok(text) => Msg::Lsp(LspReply::Hover { path, text }),
                // A failed hover is not worth a toast: it happens constantly
                // while a server is still indexing.
                Err(_) => Msg::Lsp(LspReply::Hover { path, text: None }),
            }
        });
    }

    pub fn lsp_definition(
        &mut self,
        path: &std::path::Path,
        position: crate::lsp::protocol::Position,
    ) {
        let path = path.to_path_buf();
        let lsp = self.lsp.clone();
        self.editor.busy += 1;
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Failed("no language server for this file".into()));
            };
            match client.definition(&path, position) {
                Ok(locations) => Msg::Lsp(LspReply::Definition(locations)),
                Err(e) => Msg::Lsp(LspReply::Failed(e)),
            }
        });
    }

    pub fn lsp_references(
        &mut self,
        path: &std::path::Path,
        position: crate::lsp::protocol::Position,
    ) {
        let path = path.to_path_buf();
        let lsp = self.lsp.clone();
        self.editor.busy += 1;
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Failed("no language server for this file".into()));
            };
            match client.references(&path, position) {
                Ok(locations) => Msg::Lsp(LspReply::References(locations)),
                Err(e) => Msg::Lsp(LspReply::Failed(e)),
            }
        });
    }

    pub fn lsp_symbols(&mut self, path: &std::path::Path) {
        let path = path.to_path_buf();
        let lsp = self.lsp.clone();
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else { return Msg::Noop };
            match client.document_symbols(&path) {
                Ok(symbols) => Msg::Lsp(LspReply::Symbols { path, symbols }),
                Err(_) => Msg::Noop,
            }
        });
    }

    pub fn lsp_completion(
        &mut self,
        path: &std::path::Path,
        position: crate::lsp::protocol::Position,
        anchor: usize,
    ) {
        let path = path.to_path_buf();
        let lsp = self.lsp.clone();
        self.editor.completion.requesting = true;
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Failed("no language server for this file".into()));
            };
            match client.completion(&path, position) {
                Ok(items) => Msg::Lsp(LspReply::Completion { path, items, anchor }),
                Err(e) => Msg::Lsp(LspReply::Failed(e)),
            }
        });
    }

    /// Formats a buffer. `save` continues into a write once the formatted
    /// text comes back.
    fn lsp_format(&mut self, path: &std::path::Path, text: &str, save: bool) {
        let (path, text) = (path.to_path_buf(), text.to_string());
        let lsp = self.lsp.clone();
        self.editor.busy += 1;
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Formatted { path, text, save });
            };
            // Format the text the buffer actually has, not what the server
            // last heard about.
            let _ = client.did_change(&path, &text);
            match client.format(&path, &text, 4) {
                Ok(Some(formatted)) => {
                    Msg::Lsp(LspReply::Formatted { path, text: formatted, save })
                }
                // No edits, or no formatter: saving still has to happen.
                Ok(None) => Msg::Lsp(LspReply::Formatted { path, text, save }),
                // Never let a formatter failure lose a save: write what the
                // buffer has rather than reporting and dropping it.
                Err(_) if save => Msg::Lsp(LspReply::Formatted { path, text, save }),
                Err(e) => Msg::Lsp(LspReply::Failed(e)),
            }
        });
    }

    /// Asks for a workspace-wide rename. The edits come back as a proposal:
    /// nothing is written until the user accepts them.
    pub fn lsp_rename(
        &mut self,
        path: &std::path::Path,
        position: crate::lsp::protocol::Position,
        new_name: &str,
    ) {
        let (path, new_name) = (path.to_path_buf(), new_name.to_string());
        let lsp = self.lsp.clone();
        self.editor.busy += 1;
        self.worker.spawn(move || {
            let Some(client) = lsp.running_for(&path) else {
                return Msg::Lsp(LspReply::Failed("no language server for this file".into()));
            };
            match client.rename(&path, position, &new_name) {
                Ok(edits) => Msg::Lsp(LspReply::Renamed { new_name, edits }),
                Err(e) => Msg::Lsp(LspReply::Failed(e)),
            }
        });
    }

    /// Stops every language server. They start again on the next file that
    /// needs one, which is how a wedged server gets fixed.
    pub fn lsp_restart(&mut self) {
        let lsp = self.lsp.clone();
        for file in &mut self.editor.files {
            file.synced = false;
        }
        let paths: Vec<std::path::PathBuf> =
            self.editor.files.iter().map(|f| f.path.clone()).collect();
        let texts: Vec<String> = self.editor.files.iter().map(|f| f.text.clone()).collect();
        self.toast("Restarting language servers…", false);
        self.worker.spawn(move || {
            lsp.shutdown_all();
            lsp.forget_failures();
            for (path, text) in paths.iter().zip(texts) {
                if let Ok(client) = lsp.ensure_for(path) {
                    let _ = client.did_open(path, &text);
                }
            }
            Msg::Done { message: Ok("Language servers restarted.".into()), refresh: false }
        });
    }

    // -- coding agent ---------------------------------------------------------

    /// Starts a coding run on the task in the agent tab.
    ///
    /// What the model can do is decided here, not by the model: read and
    /// edit always; the language server when one is available; the
    /// repository's own checks only in "let it iterate" mode, where its
    /// edits are on disk for those checks to actually test.
    pub fn start_coding_agent(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        if self.coding.running {
            return;
        }
        let task = self.coding.task.trim().to_string();
        if task.is_empty() {
            return;
        }
        let Some(sel) = self.ai_selection(worker::AiTarget::Coding) else {
            self.toast("No AI model selected. Pick one next to the task box.", true);
            return;
        };

        let live = self.coding.iterate;
        let history = self.coding.turns();
        let branch = self.status.as_ref().map(|s| s.branch.clone());
        let instructions = self.coding_instructions();
        // Only the checks this repository already declares, and only when
        // there is something on disk for them to check.
        let checks = if live { self.local_ci.jobs.clone() } else { Vec::new() };
        let url = self.effective_ollama_url();
        let lsp = self.lsp.clone();

        self.coding.running = true;
        self.coding.log.clear();
        self.coding.summary.clear();
        self.coding.error = None;
        self.coding.edits.clear();
        self.coding.selected = None;
        self.coding.truncated = false;
        self.coding.live = live;
        self.tab = Tab::Agent;

        let progress = self.worker.progress();
        let run_task = task.clone();
        self.worker.spawn(move || {
            let result = (|| -> Result<AgentReport, String> {
                let provider = agent_provider(&sel, &url)?;
                let tracked = strerr(repo.tracked_files())?;
                let mut workspace = crate::agent::Workspace::new(
                    repo.path(),
                    tracked,
                    crate::agent::Access::ReadWrite,
                )?
                .with_language_support(lsp)
                .with_write_mode(if live {
                    crate::agent::WriteMode::Live
                } else {
                    crate::agent::WriteMode::Overlay
                })
                .with_checks(checks);

                let run = crate::agent::coding::run(
                    provider.as_ref(),
                    &mut workspace,
                    crate::agent::coding::Request {
                        task: &run_task,
                        history: &history,
                        branch: branch.as_deref(),
                        instructions: instructions.as_deref(),
                        limits: crate::agent::coding::limits(),
                    },
                    &mut |event| {
                        progress.send(Msg::AgentEvent {
                            kind: AgentKind::Coding,
                            line: event.line(),
                        })
                    },
                )?;
                Ok(AgentReport {
                    summary: run.text,
                    edits: run.edits,
                    truncated: run.truncated,
                })
            })();
            Msg::AgentDone { kind: AgentKind::Coding, result }
        });
    }

    /// Guidance for the coding agent: the repository's own review
    /// instructions, which is where a project already writes down how its
    /// code is supposed to look.
    fn coding_instructions(&self) -> Option<String> {
        let repo = self.repo.as_ref()?;
        let cfg = self.review.config.resolve_files(repo.path()).ok()?;
        cfg.instructions
    }

    /// Files the run changed, ready for review.
    fn finish_coding_run(&mut self, result: Result<AgentReport, String>) {
        self.coding.running = false;
        let task = std::mem::take(&mut self.coding.task);
        match result {
            Ok(report) => {
                self.coding.truncated = report.truncated;
                self.coding.summary = report.summary.clone();
                self.coding.edits = report
                    .edits
                    .into_iter()
                    .map(|edit| ProposedEdit {
                        edit,
                        // In live mode the change is already on disk, so the
                        // tick means "revert this one" and starts clear.
                        accepted: false,
                        applied: false,
                        unresolved: false,
                    })
                    .collect();
                self.coding.selected = (!self.coding.edits.is_empty()).then_some(0);
                self.coding.history.push(agent_tab::Exchange {
                    task,
                    summary: report.summary,
                    changed: self.coding.edits.len(),
                });
                // A live run already changed the working tree.
                if self.coding.live {
                    self.refresh();
                }
            }
            Err(e) => {
                // Keep the task so it can be retried or edited.
                self.coding.task = task;
                self.coding.error = Some(e.clone());
                self.toast(e, true);
            }
        }
    }

    /// Writes the proposals the user ticked (overlay mode).
    pub fn apply_coding_edits(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let root = repo.path().to_path_buf();
        let mut applied = 0usize;
        let mut errors = Vec::new();
        for proposed in &mut self.coding.edits {
            if !proposed.accepted || proposed.applied {
                continue;
            }
            match write_worktree_file(&root, &proposed.edit.path, &proposed.edit.after) {
                Ok(()) => {
                    proposed.applied = true;
                    proposed.accepted = false;
                    applied += 1;
                }
                Err(e) => errors.push(format!("{}: {e}", proposed.edit.path)),
            }
        }
        if errors.is_empty() {
            self.toast(format!("Applied {applied} change(s)."), false);
        } else {
            self.toast(format!("Applied {applied}; failed: {}", errors.join("; ")), true);
        }
        self.reload_changed_buffers();
        self.refresh();
    }

    /// Restores the ticked files to what they were before the run (live
    /// mode). A file the run created is deleted.
    pub fn revert_coding_edits(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let root = repo.path().to_path_buf();
        let mut reverted = 0usize;
        let mut errors = Vec::new();
        let mut done: Vec<String> = Vec::new();

        for proposed in &mut self.coding.edits {
            if !proposed.accepted || proposed.applied {
                continue;
            }
            let path = root.join(&proposed.edit.path);
            let result = match &proposed.edit.before {
                Some(before) => std::fs::write(&path, before).map_err(|e| e.to_string()),
                None => std::fs::remove_file(&path).map_err(|e| e.to_string()),
            };
            match result {
                Ok(()) => {
                    reverted += 1;
                    done.push(proposed.edit.path.clone());
                }
                Err(e) => errors.push(format!("{}: {e}", proposed.edit.path)),
            }
        }
        // A reverted change is gone, not "applied": drop it from the list.
        self.coding.edits.retain(|e| !done.contains(&e.edit.path));
        self.coding.selected = (!self.coding.edits.is_empty()).then_some(0);

        if errors.is_empty() {
            self.toast(format!("Reverted {reverted} file(s)."), false);
        } else {
            self.toast(format!("Reverted {reverted}; failed: {}", errors.join("; ")), true);
        }
        self.reload_changed_buffers();
        self.refresh();
    }

    /// Accepts every remaining live change and clears the review list.
    pub fn keep_coding_edits(&mut self) {
        let kept = self.coding.edits.iter().filter(|e| !e.applied).count();
        for proposed in &mut self.coding.edits {
            proposed.applied = true;
            proposed.accepted = false;
        }
        self.toast(format!("Kept {kept} change(s)."), false);
        self.reload_changed_buffers();
        self.refresh();
    }

    /// Opens the selected proposal's file in the editor.
    pub fn open_selected_coding_edit(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(edit) = self
            .coding
            .selected
            .and_then(|i| self.coding.edits.get(i))
            .map(|p| p.edit.path.clone())
        else {
            return;
        };
        self.editor_open(&repo.path().join(edit), None);
    }

    /// Re-reads open buffers whose file changed underneath them.
    ///
    /// The agent writes files the editor may have open. A buffer showing
    /// stale text would overwrite the agent's work the next time it is
    /// saved, so clean buffers are refreshed; dirty ones are left alone and
    /// reported, because the user's unsaved edits are not ours to discard.
    fn reload_changed_buffers(&mut self) {
        let mut conflicted = Vec::new();
        for file in &mut self.editor.files {
            let Ok(disk) = std::fs::read_to_string(&file.path) else { continue };
            if disk == file.text {
                file.saved = disk;
                continue;
            }
            if file.is_dirty() {
                conflicted.push(file.rel.clone());
                continue;
            }
            file.text = disk.clone();
            file.saved = disk;
            file.cursor = 0;
            file.dirty_since = Some(std::time::Instant::now());
        }
        if !conflicted.is_empty() {
            self.toast(
                format!(
                    "Changed on disk while you had unsaved edits: {}. Your buffer was left \
                     as it is.",
                    conflicted.join(", ")
                ),
                true,
            );
        }
    }

    /// Runs the conflict-resolution harness across every conflicted file.
    ///
    /// Unlike [`Self::ai_resolve_conflict`], which shows one file's three
    /// versions to the model and takes back a merged file, this gives the
    /// model the repository: it reads whatever it needs and may propose
    /// changes to any file the merge requires touching. Every proposal comes
    /// back for the user to accept or reject — see [`Self::apply_agent_edits`].
    pub fn start_conflict_agent(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        if self.agent.running {
            return;
        }
        if self.conflicts.files.is_empty() {
            self.toast("No conflicted files to resolve.", true);
            return;
        }
        let Some(sel) = self.ai_selection(worker::AiTarget::Conflict) else {
            self.toast("No AI model selected. Pick one next to the AI button.", true);
            return;
        };
        let files: Vec<crate::agent::conflict::Brief> = self
            .conflicts
            .files
            .iter()
            .map(|f| crate::agent::conflict::Brief {
                path: f.path.clone(),
                ours: f.ours.clone(),
                theirs: f.theirs.clone(),
            })
            .collect();
        let custom = self.conflict_prompt();
        let url = self.effective_ollama_url();

        self.agent = AgentState {
            running: true,
            log: vec![format!(
                "· resolving {} file(s) with {}: {}",
                files.len(),
                sel.provider,
                sel.model
            )],
            ..Default::default()
        };
        self.dialog = Dialog::Conflicts;

        let progress = self.worker.progress();
        self.worker.spawn(move || {
            let result = (|| -> Result<AgentReport, String> {
                let provider = agent_provider(&sel, &url)?;
                let tracked = strerr(repo.tracked_files())?;
                let mut workspace = crate::agent::Workspace::new(
                    repo.path(),
                    tracked,
                    crate::agent::Access::ReadWrite,
                )?;
                let run = crate::agent::conflict::run(
                    provider.as_ref(),
                    &mut workspace,
                    &files,
                    custom.as_deref(),
                    crate::agent::conflict::limits(),
                    &mut |event| {
                        progress.send(Msg::AgentEvent {
                            kind: AgentKind::Conflict,
                            line: event.line(),
                        })
                    },
                )?;
                Ok(AgentReport {
                    summary: run.text,
                    edits: run.edits,
                    truncated: run.truncated,
                })
            })();
            Msg::AgentDone { kind: AgentKind::Conflict, result }
        });
    }

    /// Writes the proposals the user ticked, and nothing else.
    ///
    /// A conflicted file goes through [`crate::git::Repo::resolve`], which
    /// writes it and stages it as resolved. Any other file the model
    /// proposed is written to the worktree and left unstaged, so it shows up
    /// in Changes for a second look before it is committed.
    pub fn apply_agent_edits(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let conflicted: Vec<String> =
            self.conflicts.files.iter().map(|f| f.path.clone()).collect();

        let mut newly_resolved: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut applied = 0usize;

        for proposed in &mut self.agent.edits {
            if !proposed.accepted || proposed.applied {
                continue;
            }
            let path = proposed.edit.path.clone();
            let is_conflicted = conflicted.contains(&path);
            let outcome = if is_conflicted {
                repo.resolve(&path, &crate::git::Resolution::Manual(proposed.edit.after.clone()))
                    .map_err(|e| e.to_string())
            } else {
                write_worktree_file(repo.path(), &path, &proposed.edit.after)
            };
            match outcome {
                Ok(()) => {
                    proposed.applied = true;
                    applied += 1;
                    if is_conflicted {
                        newly_resolved.push(path);
                    }
                }
                Err(e) => errors.push(format!("{path}: {e}")),
            }
        }

        for path in newly_resolved {
            if !self.conflicts.resolved.contains(&path) {
                self.conflicts.resolved.push(path);
            }
        }

        if errors.is_empty() {
            self.toast(format!("Applied {applied} change(s)."), false);
        } else {
            self.toast(format!("Applied {applied}; failed: {}", errors.join("; ")), true);
        }
        self.refresh();
        self.load_conflicts();
    }

    /// Starts fixing a PR's merge conflicts locally: checks out the head
    /// branch, merges origin/<base>, and opens the conflict resolver.
    pub fn fix_pr_conflicts(&mut self, head: String, base: String) {
        let Some(repo) = self.repo.clone() else { return };
        self.dialog = Dialog::None;
        self.busy = true;
        self.toast(format!("Preparing conflict fix: {head} <- {base}…"), false);
        self.worker.spawn(move || {
            Msg::MergeOutcome(repo.start_pr_conflict_fix(&head, &base))
        });
    }

    /// Starts a "merge current branch into target" flow: checks GitHub
    /// branch protection first, then opens a confirmation dialog that
    /// warns when repository rules restrict the target.
    pub fn request_merge_into(&mut self, target: &str) {
        let Some(repo) = self.repo.clone() else { return };
        let source = self.status.as_ref().map(|s| s.branch.clone()).unwrap_or_default();
        let target = target.to_string();
        self.busy = true;
        self.worker.spawn(move || {
            let protected = (|| -> Option<bool> {
                let client = github::Client::from_store()?;
                let slug = views::origin_slug(&repo)?;
                client.branch_protected(&slug, &target).ok()
            })()
            .unwrap_or(false); // offline/signed out: no warning, plain confirm
            Msg::MergePrompt { source, target, protected }
        });
    }

    /// Opens the confirmation gate for a destructive action.
    pub fn confirm(&mut self, action: ConfirmAction) {
        self.dialog = Dialog::Confirm(action);
    }

    /// Executes a confirmed destructive action.
    pub fn execute_confirmed(&mut self, action: ConfirmAction) {
        self.dialog = Dialog::None;
        // Overwrite-AI-text needs no repo mutation, handle before repo gate.
        if let ConfirmAction::OverwriteAiText(target) = &action {
            match target {
                worker::AiTarget::Commit => self.generate_ai_message(),
                worker::AiTarget::PullRequest => self.generate_pr_text(),
                // The harness tasks write no text field, so nothing can be
                // overwritten and this gate never fires for them.
                worker::AiTarget::Conflict
                | worker::AiTarget::Review
                | worker::AiTarget::Coding => {}
            }
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        #[allow(clippy::match_same_arms)]
        match action {
            // Handled above; kept for exhaustiveness.
            ConfirmAction::OverwriteAiText(_) => {}
            ConfirmAction::DiscardFile(path) => {
                if self.selected_file.as_deref() == Some(path.as_str()) {
                    views::clear_diff_view(self);
                }
                self.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.discard(std::slice::from_ref(&path))
                            .map(|_| format!("Discarded changes to {path}")),
                    ),
                    refresh: true,
                });
            }
            ConfirmAction::DropStash(index) => {
                self.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.stash_drop(index).map(|_| "Stash dropped.".to_string()),
                    ),
                    refresh: true,
                });
            }
            ConfirmAction::DeleteBranch(name) => {
                self.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.delete_branch(&name, false).map(|_| format!("Deleted {name}")),
                    ),
                    refresh: true,
                });
            }
            ConfirmAction::AbortMerge => {
                self.worker.spawn(move || Msg::Done {
                    message: strerr(repo.merge_abort().map(|_| "Merge aborted.".to_string())),
                    refresh: true,
                });
            }
            ConfirmAction::AbortRebase => {
                self.worker.spawn(move || Msg::Done {
                    message: strerr(repo.rebase_abort().map(|_| "Rebase aborted.".to_string())),
                    refresh: true,
                });
            }
            ConfirmAction::UndoCommit(_) => {
                self.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.undo_last_commit()
                            .map(|_| "Commit undone. Changes kept staged.".to_string()),
                    ),
                    refresh: true,
                });
            }
            ConfirmAction::RevertCommit { sha, .. } => {
                self.worker.spawn(move || Msg::MergeOutcome(repo.revert_commit(&sha)));
            }
            ConfirmAction::MergeInto { target, .. } => {
                self.busy = true;
                self.worker.spawn(move || Msg::MergeOutcome(repo.merge_into(&target)));
            }
            ConfirmAction::DiscardAll(_) => {
                views::clear_diff_view(self);
                let paths: Vec<String> = self
                    .status
                    .as_ref()
                    .map(|s| s.files.iter().map(|f| f.path.clone()).collect())
                    .unwrap_or_default();
                self.worker.spawn(move || Msg::Done {
                    message: strerr(
                        repo.discard(&paths).map(|_| "All changes discarded.".to_string()),
                    ),
                    refresh: true,
                });
            }
        }
    }

    /// Switches branch. With uncommitted changes present, opens a dialog
    /// asking whether to bring them along or stash them first.
    pub fn request_checkout(&mut self, name: &str) {
        let dirty = self.status.as_ref().map(|s| !s.files.is_empty()).unwrap_or(false);
        if dirty {
            self.dialog = Dialog::SwitchBranch(name.to_string());
        } else {
            self.checkout_now(name, CheckoutMode::Plain);
        }
    }

    /// Performs the checkout in the chosen mode.
    pub fn checkout_now(&mut self, name: &str, mode: CheckoutMode) {
        let Some(repo) = self.repo.clone() else { return };
        let name = name.to_string();
        self.dialog = Dialog::None;
        self.worker.spawn(move || {
            let result = (|| -> Result<String, crate::git::GitError> {
                match mode {
                    CheckoutMode::Plain | CheckoutMode::Bring => {
                        // Git carries uncommitted changes across checkout and
                        // refuses when they would be overwritten.
                        repo.checkout(&name)?;
                        Ok(format!("Switched to {name}"))
                    }
                    CheckoutMode::Stash => {
                        repo.stash_save(&format!("auto-stash before switching to {name}"))?;
                        repo.checkout(&name)?;
                        Ok(format!(
                            "Changes stashed, switched to {name}. \
                             Restore them from the branch menu's Stashes."
                        ))
                    }
                }
            })();
            Msg::Done { message: strerr(result), refresh: true }
        });
    }

    /// Loads the all-branches commit log and lays out the graph.
    pub fn load_graph(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        self.worker.spawn(move || {
            let commits = repo.log_all(300).unwrap_or_default();
            Msg::Graph(graph::layout(&commits))
        });
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
                Action::QuickOpen => self.editor_quick_open(),
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
        if action == "push" {
            let set_upstream = !self.status.as_ref().map(|s| s.has_upstream).unwrap_or(false);
            self.push_with_ci("push", set_upstream);
            return;
        }
        let token = self.gh_token();
        self.worker.spawn(move || {
            let auth = token.as_deref();
            let result = repo
                .pull(crate::git::PullStrategy::FastForwardOnly, auth)
                .map(|_| "Pulled.".to_string());
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
                Ok(None) => Msg::Noop, // still pending; UI re-arms polling
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
        if self.graph_open {
            graph::draw_side_panel(self, ctx);
        }
        views::diff_panel(self, ctx);
        dialogs::show(self, ctx);
        views::toasts(self, ctx);
    }
}

/// A repaint callback for background threads that change state nobody
/// asked for — diagnostics arriving, indexing progress — so the UI wakes up
/// and shows them.
fn repaint_handle(ctx: &egui::Context) -> std::sync::Arc<dyn Fn() + Send + Sync> {
    let ctx = ctx.clone();
    std::sync::Arc::new(move || ctx.request_repaint())
}

/// Builds the harness provider for a task's provider/model selection, the
/// same pair the model picker writes.
pub fn agent_provider(
    sel: &AiSelection,
    ollama_url: &str,
) -> Result<Box<dyn crate::agent::Provider>, String> {
    if sel.provider == "claude" {
        return claude::Client::from_store(sel.model.clone())
            .map(|c| Box::new(c) as Box<dyn crate::agent::Provider>)
            .ok_or_else(|| "Claude is not signed in. Open Settings.".to_string());
    }
    if sel.model.trim().is_empty() {
        return Err("No Ollama model selected. Pick one in Settings.".into());
    }
    Ok(Box::new(ollama::Client::new(ollama_url).agent(sel.model.clone())))
}

/// Writes one accepted proposal into the worktree, creating parent
/// directories for a file the model added.
fn write_worktree_file(root: &std::path::Path, rel: &str, content: &str) -> Result<(), String> {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&full, content).map_err(|e| e.to_string())
}

/// Reviews `diff` with the reviewer able to read the repository.
///
/// Read-only: the review gate inspects code, it never edits it. The
/// reviewer's reading list comes back on the outcome, so a verdict can be
/// weighed against the context it was reached from.
fn review_with_repo_context(
    repo: &crate::git::Repo,
    sel: &AiSelection,
    ollama_url: &str,
    diff: &str,
    cfg: &crate::review::ReviewConfig,
) -> Result<crate::review::ReviewOutcome, String> {
    let provider = agent_provider(sel, ollama_url)?;
    let tracked = strerr(repo.tracked_files())?;
    let mut workspace =
        crate::agent::Workspace::new(repo.path(), tracked, crate::agent::Access::ReadOnly)?;
    crate::review::run_with_context(
        provider.as_ref(),
        &mut workspace,
        diff,
        cfg,
        &mut |_| {},
    )
}

/// The diff-only review: one request, no tools. Used when `repo_context` is
/// off and as the fallback for a model that cannot call tools.
fn review_single_shot(
    sel: &AiSelection,
    ollama_url: &str,
    diff: &str,
    cfg: &crate::review::ReviewConfig,
) -> Result<crate::review::ReviewOutcome, String> {
    if sel.provider == "claude" {
        let client = claude::Client::from_store(sel.model.clone())
            .ok_or("Claude is not signed in. Open Settings.")?;
        return strerr(client.review(diff, cfg));
    }
    strerr(ollama::Client::new(ollama_url).review(&sel.model, diff, cfg))
}

/// Whether a failure means "this model cannot call tools", which is worth
/// falling back for, as opposed to a real error worth reporting.
fn lacks_tool_support(error: &str) -> bool {
    let e = error.to_lowercase();
    e.contains("cannot call tools") || e.contains("does not support tools")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A repository with one file, and an app pointed at it.
    fn app_with_repo() -> (tempfile::TempDir, App, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@t.io"],
            vec!["config", "user.name", "T"],
        ] {
            let out = Command::new("git").args(&args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success());
        }
        // .txt so no language server is ever started by these tests.
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let ctx = egui::Context::default();
        let mut app = App::new(&ctx);
        app.repo = Some(crate::git::Repo::open(tmp.path()).unwrap());
        (tmp, app, file)
    }

    #[test]
    fn opening_a_file_twice_focuses_it_rather_than_rereading_disk() {
        let (_tmp, mut app, file) = app_with_repo();
        app.editor_open(&file, None);
        assert_eq!(app.editor.files.len(), 1);
        assert_eq!(app.tab, Tab::Editor);
        assert_eq!(app.editor.files[0].rel, "notes.txt");

        // An unsaved edit must survive the file being "opened" again, which
        // is what a diagnostic or a search result does.
        app.editor.files[0].text.push_str("edited\n");
        app.editor_open(&file, Some(2));
        assert_eq!(app.editor.files.len(), 1);
        assert!(app.editor.files[0].text.ends_with("edited\n"));
        assert_eq!(app.editor.files[0].reveal, Some(2));
    }

    #[test]
    fn closing_refuses_to_discard_unsaved_work() {
        let (_tmp, mut app, file) = app_with_repo();
        app.editor_open(&file, None);
        app.editor.files[0].text = "changed\n".into();
        app.editor_close(0);
        assert_eq!(app.editor.files.len(), 1, "a dirty buffer must not close silently");

        app.editor.files[0].saved = app.editor.files[0].text.clone();
        app.editor_close(0);
        assert!(app.editor.files.is_empty());
        assert_eq!(app.editor.active, None);
    }

    #[test]
    fn saving_writes_the_buffer_and_clears_the_dirty_marker() {
        let (_tmp, mut app, file) = app_with_repo();
        app.editor_open(&file, None);
        app.editor.files[0].text = "rewritten\n".into();
        assert!(app.editor.files[0].is_dirty());

        app.editor_save();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "rewritten\n");
        assert!(!app.editor.files[0].is_dirty());
    }

    #[test]
    fn closing_the_active_tab_activates_a_neighbour() {
        let (tmp, mut app, first) = app_with_repo();
        let second = tmp.path().join("other.txt");
        std::fs::write(&second, "x\n").unwrap();
        app.editor_open(&first, None);
        app.editor_open(&second, None);
        assert_eq!(app.editor.active, Some(1));

        app.editor_close(1);
        assert_eq!(app.editor.active, Some(0));
        app.editor_close(0);
        assert_eq!(app.editor.active, None);
    }

    #[test]
    fn a_rename_becomes_a_proposal_instead_of_writing_files() {
        use crate::lsp::protocol::{Position, Range, TextEdit};
        let (_tmp, mut app, file) = app_with_repo();
        let edits = vec![(
            file.clone(),
            vec![TextEdit {
                range: Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 3),
                },
                new_text: "ONE".into(),
            }],
        )];
        app.stage_rename_proposal("ONE", edits);

        assert_eq!(app.dialog, Dialog::AgentChanges);
        assert_eq!(app.agent.edits.len(), 1);
        assert_eq!(app.agent.edits[0].edit.path, "notes.txt");
        assert!(app.agent.edits[0].edit.after.starts_with("ONE\n"));
        assert!(!app.agent.edits[0].accepted, "nothing is pre-accepted");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\ntwo\nthree\n",
            "the file itself must be untouched until the user applies it"
        );
    }

    #[test]
    fn a_rename_uses_the_open_buffer_rather_than_stale_disk_contents() {
        use crate::lsp::protocol::{Position, Range, TextEdit};
        let (_tmp, mut app, file) = app_with_repo();
        app.editor_open(&file, None);
        app.editor.files[0].text = "ONE\ntwo\nthree\n".into();

        let edits = vec![(
            file,
            vec![TextEdit {
                range: Range {
                    start: Position::new(1, 0),
                    end: Position::new(1, 3),
                },
                new_text: "TWO".into(),
            }],
        )];
        app.stage_rename_proposal("TWO", edits);
        assert_eq!(app.agent.edits[0].edit.after, "ONE\nTWO\nthree\n");
    }

    /// Renders the editor tab for real, which is the only way to catch a
    /// panic in the layouter, the gutter, or an id clash.
    #[test]
    fn the_editor_tab_renders_without_panicking() {
        let (_tmp, mut app, file) = app_with_repo();
        app.editor_open(&file, None);
        app.editor.files[0].symbols = vec![crate::lsp::protocol::Symbol {
            name: "section".into(),
            kind: 12,
            range: Default::default(),
            depth: 0,
            detail: None,
        }];
        app.editor.outline_open = true;
        app.editor.completion.open = true;
        app.editor.completion.items = vec![crate::lsp::protocol::CompletionItem {
            label: "candidate".into(),
            detail: Some("fn()".into()),
            insert: "candidate".into(),
            range: None,
            sort_text: None,
            kind: Some(3),
        }];

        egui::__run_test_ctx(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                editor::editor_tab(&mut app, ui);
            });
        });
    }

    /// The agent tab renders in every state it can be in.
    #[test]
    fn the_agent_tab_renders_without_panicking() {
        let (_tmp, mut app, _file) = app_with_repo();
        app.coding.task = "add a flag".into();
        app.coding.log = vec!["· read src/main.rs".into()];
        app.coding.summary = "- did the thing".into();
        app.coding.history.push(agent_tab::Exchange {
            task: "earlier task".into(),
            summary: "earlier summary".into(),
            changed: 1,
        });
        app.coding.edits = vec![ProposedEdit {
            edit: crate::agent::PendingEdit {
                path: "notes.txt".into(),
                before: Some("one\n".into()),
                after: "ONE\n".into(),
            },
            accepted: true,
            applied: false,
            unresolved: false,
        }];
        app.coding.selected = Some(0);

        for live in [false, true] {
            app.coding.live = live;
            egui::__run_test_ctx(|ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    agent_tab::agent_tab(&mut app, ui);
                });
            });
        }
    }

    #[test]
    fn applying_a_coding_proposal_writes_only_the_ticked_files() {
        let (tmp, mut app, file) = app_with_repo();
        let other = tmp.path().join("skip.txt");
        std::fs::write(&other, "keep\n").unwrap();
        app.coding.live = false;
        app.coding.edits = vec![
            ProposedEdit {
                edit: crate::agent::PendingEdit {
                    path: "notes.txt".into(),
                    before: Some("one\n".into()),
                    after: "ONE\n".into(),
                },
                accepted: true,
                applied: false,
                unresolved: false,
            },
            ProposedEdit {
                edit: crate::agent::PendingEdit {
                    path: "skip.txt".into(),
                    before: Some("keep\n".into()),
                    after: "CHANGED\n".into(),
                },
                accepted: false,
                applied: false,
                unresolved: false,
            },
        ];

        app.apply_coding_edits();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\n");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep\n");
        assert!(app.coding.edits[0].applied);
        assert!(!app.coding.edits[1].applied);
    }

    #[test]
    fn reverting_a_live_change_restores_the_original_and_deletes_new_files() {
        let (tmp, mut app, file) = app_with_repo();
        // A live run has already written both of these.
        std::fs::write(&file, "AGENT\n").unwrap();
        let created = tmp.path().join("created.txt");
        std::fs::write(&created, "new file\n").unwrap();

        app.coding.live = true;
        app.coding.edits = vec![
            ProposedEdit {
                edit: crate::agent::PendingEdit {
                    path: "notes.txt".into(),
                    before: Some("one\ntwo\nthree\n".into()),
                    after: "AGENT\n".into(),
                },
                accepted: true,
                applied: false,
                unresolved: false,
            },
            ProposedEdit {
                edit: crate::agent::PendingEdit {
                    path: "created.txt".into(),
                    before: None,
                    after: "new file\n".into(),
                },
                accepted: true,
                applied: false,
                unresolved: false,
            },
        ];

        app.revert_coding_edits();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
        assert!(!created.exists(), "a file the run created must not survive a revert");
        assert!(app.coding.edits.is_empty(), "reverted changes leave the review list");
    }

    #[test]
    fn a_clean_buffer_follows_the_file_but_a_dirty_one_is_left_alone() {
        let (tmp, mut app, file) = app_with_repo();
        let second = tmp.path().join("second.txt");
        std::fs::write(&second, "before\n").unwrap();
        app.editor_open(&file, None);
        app.editor_open(&second, None);

        // The user is mid-edit in the second buffer.
        app.editor.files[1].text = "my unsaved work\n".into();

        // The agent rewrites both files underneath the editor.
        std::fs::write(&file, "agent wrote this\n").unwrap();
        std::fs::write(&second, "agent wrote this too\n").unwrap();
        app.reload_changed_buffers();

        assert_eq!(app.editor.files[0].text, "agent wrote this\n");
        assert!(!app.editor.files[0].is_dirty());
        assert_eq!(
            app.editor.files[1].text, "my unsaved work\n",
            "unsaved edits must never be overwritten"
        );
    }

    /// The same, with no repository and no open files: the empty states.
    #[test]
    fn the_editor_tab_renders_when_there_is_nothing_to_show() {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx);
        egui::__run_test_ctx(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                editor::editor_tab(&mut app, ui);
            });
        });

        let (_tmp, mut app, _file) = app_with_repo();
        egui::__run_test_ctx(|ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                editor::editor_tab(&mut app, ui);
            });
        });
    }
}
