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
  **Resolve all with AI** hands the whole repository to a model: it reads the
  files it needs, proposes a merge for each conflict plus any other file the
  merge requires touching, and shows you every change as a diff. Nothing is
  written until you tick it and apply.
- **Markdown, rendered**: `.md` files, AI review output, and the coding
  agent's summaries are rendered rather than dumped as source — real bold and
  italic faces, syntax-highlighted fenced code, blockquotes, lists, and links.
- **Code editor** with **language server** support: diagnostics inline, hover
  types, go-to-definition, find references, an outline, completion, format on
  save, and workspace rename. Servers start on demand (rust-analyzer, pyright,
  gopls, clangd, and more) and can be configured per repository.
- **Coding agent**: give it a task and it reads, edits, asks the language
  server what it broke, and runs your own checks until it works. Every change
  it makes is reviewed as a diff and applied — or reverted — by you.
- **GitHub**: sign in via browser device flow or a personal access token,
  authenticated push/pull/fetch, list and **create pull requests**
  (with an AI-generated title and description written from **every commit on
  the branch** — its subjects, bodies, and full diff against the base — not
  from whatever happens to be staged).
- **Stacked pull requests**: split one large change into a chain of branches,
  each PR targeting the branch below it so every reviewer sees one focused
  diff. DevDock keeps the chain in order — restack after any branch changes,
  push and open every PR in one action, write a stack map into each body, and
  drop merged branches out of the stack after they land.
  See [docs/stacked-prs.md](docs/stacked-prs.md).

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

The reviewer **reads your repository** while it reviews (tracked files only,
read-only), so it judges a change against the code around it rather than
against the diff alone, and it tells you what it opened. Turn that off with
`repo_context = false` under `[review]`.

See the full guide: [docs/local-ci.md](docs/local-ci.md)
([gating pushes](docs/local-ci.md#gating-pushes-and-pull-requests),
[AI code review](docs/local-ci.md#ai-code-review))
and the extension API: [docs/extending-local-ci.md](docs/extending-local-ci.md).

## Editor, language servers, and the coding agent

The **Editor** tab is a real editor with everything a language server
provides; the **Agent** tab takes a task and works on the repository, using
the same language server and your own CI checks to verify itself.

Both are documented in
[docs/editor-and-agent.md](docs/editor-and-agent.md) — including how to point
DevDock at a server it does not know about:

```toml
# .git-manage-ci.toml
[[lsp]]
extensions = ["nim"]
command = "nimlangserver"
```

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
  review.rs    The AI review gate: config, prompts, findings, thresholds
  stack.rs     Stacked pull requests: the parent chain, restack, submit, sync
  agent/       Tool-use harness: read, edit, language server, and check
               tools; the conflict resolver and the coding agent run on it
  lsp/         Language server client: JSON-RPC over stdio, one process per
               server, diagnostics and navigation for the editor and agent
  app/
    mod.rs     App state, config, background message pump
    theme.rs   Visual identity: colour tokens, type scale, bundled fonts
    markdown.rs  Markdown rendering for reviews, agent summaries, and .md files
    views.rs   Toolbar, sidebar, diff panel
    dialogs.rs Repo picker, GitHub, PRs, stacks, conflicts, settings
    editor.rs  Code editor: buffers, highlighting, LSP interactions
    agent_tab.rs The coding agent's task panel and change review
    worker.rs  Background thread runner
tests/
  workflow.rs  End-to-end git workflow tests against throwaway repos
  stack.rs     Stacked PRs: parent links, restacking, merge detection
  stack_live.rs  The same flow against real GitHub (ignored by default)
  agent.rs     Harness tests: sandbox limits, proposals, applied merges,
               and the coding agent against a real server and real checks
  lsp.rs       Language server client, against a real child process
```

The `git_manage` library (git/github/ollama modules) has no UI dependencies and
can be reused to build other clients.

## Development

```sh
cargo test        # unit + integration tests
cargo clippy      # lints
cargo run         # debug build
```

Some tests talk to real services and are ignored by default — a language
server, an Ollama or Claude model, and GitHub:

```sh
cargo test --test stack_live -- --ignored --nocapture
```

That one creates a private scratch repository, opens a three-branch stack of
pull requests in it, squash-merges the bottom one, syncs, and checks that the
pull request above it was rebased and retargeted. It prints the repository's
URL at the end; deleting it needs the `delete_repo` scope, which the app never
asks for, so tidy it up by hand.

## License

MIT
