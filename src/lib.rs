//! `git_manage`: a native desktop git client library and app.
//!
//! Layers:
//!
//! - [`agent`]: the tool-use harness that lets a model read the repository
//!   (code review) or propose edits across the worktree (conflict resolution),
//!   with every proposed write held for the user to confirm.
//! - [`git`]: typed, synchronous wrapper around the `git` CLI. The central
//!   type is [`git::Repo`]: status, staging, commits, diffs, branches,
//!   merge/rebase, conflict resolution, and remote sync.
//! - [`github`]: GitHub device-flow sign-in, token storage, and the pull
//!   request subset of the REST API.
//! - [`ollama`]: local Ollama client that turns diffs into commit messages.
//! - [`app`]: the egui desktop application built on top of the layers above.

pub mod agent;
pub mod app;
pub mod claude;
pub mod cli;
pub mod cli_style;
pub mod git;
pub mod github;
pub mod local_ci;
pub mod ollama;
pub mod review;
pub mod secure_store;
