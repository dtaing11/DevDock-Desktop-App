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
    /// The Jira account the stored credentials belong to, and the projects
    /// it can file into.
    JiraConnected(Result<(String, Vec<crate::jira::Project>), String>),
    /// Issue types for one project.
    JiraTypes { project: String, types: Result<Vec<crate::jira::IssueType>, String> },
    /// Tickets drafted from a list, awaiting confirmation.
    TicketDrafts(Result<crate::agent::tickets::Proposal, String>),
    /// One ticket created, or not.
    TicketCreated { index: usize, result: Result<crate::jira::Issue, String> },

    /// The unassigned tickets of a Jira project.
    BacklogIssues(Result<Vec<crate::jira::BacklogIssue>, String>),
    /// What the model concluded about each of them.
    BacklogTriage(Result<Vec<crate::agent::backlog::Triage>, String>),
    /// One line of the triage run's progress.
    BacklogTriageEvent(String),
    /// One line from a ticket's fix: a tool call, a check, a push.
    BacklogProgress { key: String, line: String },
    /// A ticket's fix finished, one way or the other.
    BacklogDone { key: String, result: Result<Box<crate::backlog::Fixed>, String> },
    /// A message for one repository's state: the one the window shows, or
    /// one kept aside while another is open. Agents keep running on a
    /// repository after the window moves on, and their messages must not
    /// land in the wrong one.
    Routed { repo: String, msg: Box<Msg> },
    /// A line from an Agent-tab run in its own worktree, keyed by branch.
    AgentRunProgress { key: String, line: String },
    /// An Agent-tab worktree run finished.
    AgentRunDone { key: String, result: Result<Box<crate::backlog::Fixed>, String> },

    /// Every checkout of the repository.
    Worktrees(Result<Vec<crate::git::Worktree>, String>),
    /// A worktree was added or removed. `open` names one to open afterwards,
    /// and whether in a new window.
    WorktreeDone {
        message: Result<String, String>,
        open: Option<(std::path::PathBuf, bool)>,
    },

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
    /// OpenCode's model list, from `opencode models`.
    OpenCodeModels(Vec<String>),
    /// Screenshots of the in-tab run's result, taken in a sandbox afterwards.
    AgentScreenshots(Result<crate::screenshots::Report, String>),
    /// An agent has a question for the developer and is waiting. `key` is
    /// the worktree run's branch, or `None` for the Agent tab's own run.
    /// The answer goes back through `reply`.
    AgentQuestion { key: Option<String>, question: String, reply: std::sync::mpsc::Sender<String> },

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
    /// The ticket writer, in its dialog.
    Tickets,
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
    /// The ticket writer, which reads the repository to draft them.
    Tickets,
    /// The backlog fixer: the coding agent, unattended, one ticket per
    /// worktree. Its own setting because it runs without anyone watching
    /// and is worth the strongest model available.
    Backlog,
}

impl AiTarget {
    /// Every task, in the order Settings lists them.
    pub const ALL: [AiTarget; 7] = [
        AiTarget::Commit,
        AiTarget::PullRequest,
        AiTarget::Review,
        AiTarget::Conflict,
        AiTarget::Coding,
        AiTarget::Tickets,
        AiTarget::Backlog,
    ];

    /// Label for the model picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => "commit messages",
            Self::PullRequest => "pull request text",
            Self::Conflict => "conflict resolution",
            Self::Review => "code review",
            Self::Coding => "the coding agent",
            Self::Tickets => "Jira tickets",
            Self::Backlog => "the backlog fixer",
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
    /// The repository the job belongs to, when its messages must reach
    /// that repository's state whichever one the window shows.
    repo: Option<String>,
}

impl Progress {
    /// Tags every message from here on with the repository it is for.
    pub fn for_repo(mut self, repo: String) -> Self {
        self.repo = Some(repo);
        self
    }

    /// A line to the developer for a run keyed by `key`: the question goes
    /// to the UI, and this blocks the worker until the answer comes or the
    /// developer has had half an hour.
    pub fn asker(&self, key: Option<String>) -> crate::agent::Asker {
        let progress = self.clone();
        std::sync::Arc::new(move |question: &str| {
            let (tx, rx) = std::sync::mpsc::channel();
            progress.send(Msg::AgentQuestion { key: key.clone(), question: question.to_string(), reply: tx });
            rx.recv_timeout(std::time::Duration::from_secs(30 * 60))
                .map_err(|_| "no answer within 30 minutes".to_string())
        })
    }

    /// Delivers one message to the UI thread and asks for a repaint.
    pub fn send(&self, msg: Msg) {
        let msg = match &self.repo {
            Some(repo) => Msg::Routed { repo: repo.clone(), msg: Box::new(msg) },
            None => msg,
        };
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
        Progress { tx: self.tx.clone(), ctx: self.ctx.clone(), repo: None }
    }

    /// Like [`Self::spawn`], with the job's message tagged for `repo`, so
    /// it lands in that repository's state even if another is open by then.
    pub fn spawn_for(&self, repo: String, job: impl FnOnce() -> Msg + Send + 'static) {
        self.spawn(move || Msg::Routed { repo, msg: Box::new(job()) })
    }

    /// Runs `job` on a new thread and delivers its message to the UI.
    /// A handle for sending progress from inside a job.
    pub fn sender(&self) -> std::sync::mpsc::Sender<Msg> {
        self.tx.clone()
    }

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
