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

    OllamaModels(Result<Vec<Model>, String>),
    /// AI-generated text for the commit box or the PR form.
    AiSuggestion { target: AiTarget, result: Result<CommitSuggestion, String> },
    /// AI-proposed merged content for one conflicted file. Never applied
    /// automatically: the user must review and confirm in the resolver.
    AiMergeProposal { path: String, result: Result<String, String> },

    /// Background task finished with nothing to report.
    Noop,
}

/// Where an AI suggestion should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiTarget {
    /// Commit summary/description fields.
    Commit,
    /// Pull request title/body fields.
    PullRequest,
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
