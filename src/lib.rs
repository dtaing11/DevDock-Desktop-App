//! `git_manage`: a native desktop git client library and app.
//!
//! Layers:
//!
//! - [`git`]: typed, synchronous wrapper around the `git` CLI. The central
//!   type is [`git::Repo`]: status, staging, commits, diffs, branches,
//!   merge/rebase, conflict resolution, and remote sync.
//! - [`github`]: GitHub device-flow sign-in, token storage, and the pull
//!   request subset of the REST API.
//! - [`ollama`]: local Ollama client that turns diffs into commit messages.
//! - [`app`]: the egui desktop application built on top of the layers above.

pub mod app;
pub mod claude;
pub mod git;
pub mod github;
pub mod local_ci;
pub mod ollama;
