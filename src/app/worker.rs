//! Background worker: runs blocking operations off the UI thread.
//!
//! GUI code calls [`Worker::spawn`] with a closure producing a [`Msg`]; the
//! result is delivered to the UI thread via an mpsc channel and a repaint
//! request. This keeps the egui update loop responsive during git and
//! network operations.

use crate::git::{Branch, BranchList, Commit, CommitFileChange, ConflictFile, Hunk, OpOutcome, StashEntry, Status};
use crate::github::{ChecksSummary, DeviceCode, PullRequest, RemoteRepo, User};
use crate::ollama::{CommitSuggestion, Model};
use std::sync::mpsc::{Receiver, Sender};

/// Messages sent from background tasks back to the UI thread.
#[derive(Debug)]
pub enum Msg {
    /// Repository opened (or open failed).
    RepoOpened(Result<String, String>),
    Status(Result<Status, String>),
    Branches(Result<BranchList, String>),
    Log(Result<Vec<Commit>, String>),
    Diff { title: String, text: String },
    /// The working-tree text of a file, for the Markdown preview.
    Preview { path: String, text: String },
    /// A git operation finished; message shown as a toast. `refresh` reloads state.
    Done { message: Result<String, String>, refresh: bool },
    MergeOutcome(OpOutcome),
    Conflicts(Result<Vec<ConflictFile>, String>),
    Stashes(Result<Vec<StashEntry>, String>),
    Hunks { file: String, hunks: Vec<Hunk> },
    CommitFiles { sha: String, files: Vec<CommitFileChange> },
    GhRepos(Result<Vec<RemoteRepo>, String>),
    Tags(Result<Vec<String>, String>),
    /// Claude models available to the signed-in account.
    ClaudeModels(Vec<String>),
    /// One local CI job finished.
    CiJobDone { index: usize, result: crate::local_ci::JobResult },
    /// The AI reviewer finished. `Err` means the review could not be
    /// produced, which reports but never blocks the gated action.
    ReviewDone(Result<crate::review::ReviewOutcome, String>),

    GhDeviceCode(Result<DeviceCode, String>),
    GhSignedIn(Result<User, String>),
    GhUser(Option<User>),
    GhPrs(Result<Vec<PullRequest>, String>),
    GhPrCreated(Result<PullRequest, String>),
    /// CI checks for the current branch's head (branch name, summary).
    GhBranchChecks { branch: String, summary: ChecksSummary },
    /// CI checks for the default branch (main/master).
    GhMainChecks { branch: String, summary: ChecksSummary },
    /// Result of the pre-merge inspection: open the confirm dialog.
    MergePrompt { source: String, target: String, protected: bool },
    /// Laid-out commit graph nodes (all branches).
    Graph(Vec<crate::app::graph::GraphNode>),
    /// CI checks for one PR head SHA.
    GhPrChecks { number: u64, summary: ChecksSummary },
    /// Mergeable state for one PR (None = GitHub still computing).
    GhPrMergeable { number: u64, mergeable: Option<bool> },
    /// Changed files + submitted reviews for the PR being reviewed.
    GhPrReviewData {
        number: u64,
        files: Vec<crate::github::PrFile>,
        reviews: Vec<crate::github::PrReview>,
    },

    OllamaModels(Result<Vec<Model>, String>),
    /// AI-generated text for the commit box or the PR form.
    AiSuggestion { target: AiTarget, result: Result<CommitSuggestion, String> },
    /// AI-proposed merged content for one conflicted file. Never applied
    /// automatically: the user must review and confirm in the resolver.
    AiMergeProposal { path: String, result: Result<String, String> },
    /// AI-drafted local CI config (TOML). Never written automatically:
    /// the user reviews and confirms in a dialog first.
    AiCiConfig { result: Result<String, String> },
    /// Every file git tracks, for the editor's file finder.
    TrackedFiles(Vec<String>),
    /// Matching lines from a project-wide content search.
    SearchHits(Vec<crate::git::GrepHit>),
    /// Recent `HEAD` movements, for the undo dialog.
    Reflog(Result<Vec<crate::git::ReflogEntry>, String>),
    /// The branch chain the current branch sits in.
    Stack(Result<crate::stack::Stack, String>),
    /// A stack operation finished: a toast message, the per-branch log the
    /// stack view keeps, and whether it stopped in conflict.
    StackDone { message: Result<String, String>, log: Vec<String>, conflicted: bool },
    /// A proposed split of the working tree into commits.
    SplitProposal(Result<crate::agent::split::Proposal, String>),
    /// A proposed rewrite of the branch, with the commits it was built from.
    TidyProposal(
        Result<(crate::agent::rebase::Proposal, Vec<crate::git::Commit>), String>,
    ),
    /// A language server answered, or failed to.
    Lsp(LspReply),
    /// One step of an agentic run (a file read, an edit proposed), for the
    /// progress log the user watches while it works.
    AgentEvent { kind: AgentKind, line: String },
    /// The agent wrote or updated its plan.
    AgentPlan { kind: AgentKind, steps: Vec<crate::agent::PlanStep> },
    /// An agentic run finished. Its edits are proposals: the user accepts or
    /// rejects each one before anything is written.
    AgentDone { kind: AgentKind, result: Result<crate::app::AgentReport, String> },

    /// Background task finished with nothing to report.
    Noop,
}

/// An answer from a language server, on its way back to the UI thread.
///
/// Every request runs on a worker: `rust-analyzer` can take a minute to
/// answer while it indexes, and a render pass cannot wait for that.
#[derive(Debug)]
pub enum LspReply {
    /// A file was handed to a server (or could not be).
    Opened { path: std::path::PathBuf, server: String, error: Option<String> },
    Hover { path: std::path::PathBuf, text: Option<String> },
    Definition(Vec<crate::lsp::protocol::Location>),
    References(Vec<crate::lsp::protocol::Location>),
    Symbols { path: std::path::PathBuf, symbols: Vec<crate::lsp::protocol::Symbol> },
    Completion {
        path: std::path::PathBuf,
        items: Vec<crate::lsp::protocol::CompletionItem>,
        anchor: usize,
    },
    /// Formatted text for a buffer. `save` carries the format-then-save flow.
    Formatted { path: std::path::PathBuf, text: String, save: bool },
    /// A rename's edits, grouped by file. Nothing is written yet.
    Renamed {
        new_name: String,
        edits: Vec<(std::path::PathBuf, Vec<crate::lsp::protocol::TextEdit>)>,
    },
    /// A request failed; the message is user-presentable.
    Failed(String),
}

/// Which agentic run a message belongs to. Both run the same harness, and
/// both can be in flight at once, so their progress must not land in the
/// same log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// The conflict resolver, in the conflict dialog.
    Conflict,
    /// The coding agent, in its own tab.
    Coding,
}

/// Which AI task a provider/model selection belongs to. Each task picks its
/// own model, so a small local model can write commit messages while a
/// stronger one reviews code or resolves conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiTarget {
    /// Commit summary/description fields.
    Commit,
    /// Pull request title/body fields.
    PullRequest,
    /// The conflict-resolution harness, which edits files.
    Conflict,
    /// The code review gate, which reads them.
    Review,
    /// The coding agent, which does both and runs checks.
    Coding,
}

impl AiTarget {
    /// Label for the model picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => "commit messages",
            Self::PullRequest => "pull request text",
            Self::Conflict => "conflict resolution",
            Self::Review => "code review",
            Self::Coding => "the coding agent",
        }
    }
}

/// A channel back to the UI for a job that reports progress as it runs,
/// rather than only when it finishes. Cheap to clone and `Send`, so it can
/// be handed to a long agentic run on a worker thread.
#[derive(Clone)]
pub struct Progress {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Progress {
    /// Delivers one message to the UI thread and asks for a repaint.
    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

/// Handle for spawning background tasks that report back as [`Msg`]s.
pub struct Worker {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Worker {
    /// Creates a worker plus the receiver the UI thread drains each frame.
    pub fn new(ctx: egui::Context) -> (Self, Receiver<Msg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx, ctx }, rx)
    }

    /// A progress handle for jobs that report as they go.
    pub fn progress(&self) -> Progress {
        Progress { tx: self.tx.clone(), ctx: self.ctx.clone() }
    }

    /// Runs `job` on a new thread and delivers its message to the UI.
    pub fn spawn(&self, job: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let msg = job();
            // Receiver is only dropped on shutdown; ignore send failures then.
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }
}

/// Convenience: formats any displayable error into the `Result<_, String>`
/// shape carried by [`Msg`].
pub fn strerr<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, String> {
    r.map_err(|e| e.to_string())
}

/// Branches from local + remote lists, current first, for pickers.
pub fn pickable_branches(list: &BranchList) -> Vec<Branch> {
    list.local.iter().chain(list.remote.iter()).filter(|b| !b.current).cloned().collect()
}
