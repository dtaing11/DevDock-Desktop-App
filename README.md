# DevDock

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

### Prebuilt packages

Grab the latest from [Releases](https://github.com/dtaing11/DevDock-Desktop-App/releases):

- **Linux**: `devdock_*.deb` (`sudo dpkg -i devdock_*.deb`) or the `.tar.gz` binary
- **macOS**: `DevDock-macOS.zip`, unzip and drag `DevDock.app` to Applications

Releases are built automatically when a version tag (`v*`) is pushed.

### From source

Prerequisites: `git` on PATH, Rust toolchain, and on Linux the usual GUI deps:

```sh
# Debian/Ubuntu
sudo apt install build-essential libgtk-3-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev

cargo build --release
./target/release/devdock
```

Optional targets:

- `make install`: binary + `.desktop` entry + icon (Linux)
- `make app`: builds `dist/DevDock.app` (macOS bundle)
- `make deb`: builds a Debian package (needs `cargo install cargo-deb`)

## AI setup

AI features (commit messages, PR text, conflict resolution, and the
[code reviewer](docs/local-ci.md#ai-code-review)) need a model. Set up either
provider — DevDock does not ship one.

### Ollama (local)

1. Install [Ollama](https://ollama.com) and pull a model your machine can run,
   e.g. `ollama pull llama3.2`. Confirm with `ollama list`.
2. In the app, open **Settings (⚙)**, confirm the server URL
   (default `http://localhost:11434`), and pick a model.
3. Click **✨ AI message** in the commit box. The model reads the staged diff and
   fills in the summary and description.

Pick a model that fits your RAM/VRAM. Code review in particular sends a whole
diff, so an oversized model will be slow or fail to load.

### Claude (Anthropic)

Open **Settings (⚙)** → the Claude section, then either:

- **Browser sign-in** — approve access in the tab that opens and paste the code
  shown afterwards. Uses your Claude subscription.
- **API key** — paste a key (`sk-ant-…`) from
  [console.anthropic.com](https://console.anthropic.com). Bills per token.

A subscription meters Opus and Sonnet far lower than Haiku; if you hit the cap
the client falls back to Haiku so the request still completes. Use an API key to
stay on a large model consistently.

## CLI

The same binary is a full CLI: `devdock status`, `log`, `branches`,
`stash`, `commit --ai`, `push` (CI-gated), `pr --ai` (opens a pull
request), `ci`, and `hook`. See [docs/cli.md](docs/cli.md).

## Local CI (checks before a PR)

Define per-repo checks in `.git-manage-ci.toml` and run them from the
Pull Request dialog, on your machine or inside Docker containers, with
secrets support. Add `[on_push]` to gate pushes on them, and `[review]` to
have an AI review the outgoing diff first — it reports findings with its
reasoning, and you can always proceed anyway. The reviewer needs a model set
up first (see [AI setup](#ai-setup)).

See the full guide: [docs/local-ci.md](docs/local-ci.md)
([gating pushes](docs/local-ci.md#gating-pushes-and-pull-requests),
[AI code review](docs/local-ci.md#ai-code-review))
and the extension API: [docs/extending-local-ci.md](docs/extending-local-ci.md).

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
