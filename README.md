# Git Manage

A **native desktop git client** (no webview) with GitHub and Ollama integration.
Built in Rust with [egui](https://github.com/emilk/egui). Runs on Linux (also
macOS/Windows since egui is cross-platform).

![Rust](https://img.shields.io/badge/rust-stable-orange) ![License](https://img.shields.io/badge/license-MIT-blue)

## Features

- **Changes view**: see modified/added/deleted/renamed files, check exactly which
  files go into each commit, view diffs, discard changes.
- **Commit**: summary + description, with **AI-generated commit messages** from a
  local [Ollama](https://ollama.com) model reading your diff.
- **History**: browse commits and their patches.
- **Branches**: create, switch, filter local and remote branches.
- **Sync**: fetch, pull, push (auto-publishes new branches).
- **Merge & rebase** with a built-in **conflict resolver**
  (take ours / take theirs / manual editing), including rebase continue/abort.
- **GitHub**: sign in via browser device flow or a personal access token,
  authenticated push/pull/fetch, list and **create pull requests**
  (with AI-generated PR title/body).

## Install

Prerequisites: `git` on PATH, Rust toolchain, and on Linux the usual GUI deps:

```sh
# Debian/Ubuntu
sudo apt install build-essential libgtk-3-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev

cargo build --release
./target/release/git-manage
```

Optional: `make install` installs the binary plus a `.desktop` entry.

## Ollama setup

1. Install [Ollama](https://ollama.com) and pull a model, e.g. `ollama pull llama3.2`.
2. In the app, open **Settings (⚙)**, confirm the server URL
   (default `http://localhost:11434`), and pick a model.
3. Click **✨ AI message** in the commit box. The model reads the staged diff and
   fills in the summary and description.

## GitHub sign-in

Click the **🐙** button. Either:
- **Browser sign-in**: a device code is copied for you and the verification page
  opens; enter the code to authorize, or
- **Personal access token**: paste a token with `repo` scope.

Tokens are stored at `~/.config/git-manage/auth.json` (mode 600).

## Architecture

```
src/
  git.rs       Typed wrapper around the git CLI (library, reusable)
  github.rs    Device-flow auth + PR REST API (library, reusable)
  ollama.rs    Commit-message generation client (library, reusable)
  app/
    mod.rs     App state, config, background message pump
    theme.rs   Visual identity (indigo/ember/teal, not a GitHub clone)
    views.rs   Toolbar, sidebar, diff panel
    dialogs.rs Repo picker, GitHub, PRs, conflicts, settings
    worker.rs  Background thread runner
tests/
  workflow.rs  End-to-end git workflow tests against throwaway repos
```

The `git_manage` library (git/github/ollama modules) has no UI dependencies and
can be reused to build other clients.

## Development

```sh
cargo test        # unit + integration tests
cargo clippy      # lints
cargo run         # debug build
```

## License

MIT
