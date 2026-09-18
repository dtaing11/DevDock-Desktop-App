# DevDock

A **native desktop git client** (no webview) that runs coding agents on your
repositories: in your tree, in worktrees of their own, on the Jira backlog,
inside a Linux sandbox, with two engines — its own harness, or Claude Code.
Built in Rust with [egui](https://github.com/emilk/egui). Runs on macOS and
Linux (Windows too, egui being cross-platform).

![Rust](https://img.shields.io/badge/rust-stable-orange) ![License](https://img.shields.io/badge/license-MIT-blue)

> Building from source? After `cargo build --release` (or `devdock push`,
> which builds it), run `target/release/devdock self-install` so the
> `devdock` on your PATH is the build you just made. `devdock version`
> says which build is running.

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
  server what it broke, runs your own checks and any shell command it needs,
  and asks *you* — in a box on the run's card — when something genuinely
  needs a decision. Attach images to the task: a screenshot, a mockup. Every
  change is reviewed as a diff and applied — or reverted — by you.
- **Three engines**: DevDock's own harness (any Claude model, or Ollama),
  **Claude Code** run headless, or **OpenCode** (open source, any provider;
  its own models are free) — all with the repository's MCP servers, the
  same questions and images, and a session resumed with your answer. Pick
  per task in Settings; the fixer and the reviewer can be different engines.
- **A sandbox that is a machine of the run's own**: a Lima VM, an Apple
  container, or Docker — network on, root shell, toolchains installed by
  DevDock (Flutter, Rust, Node, Python, Go…) and kept between runs. Checks,
  the agent's commands, MCP servers, and Claude Code itself run inside.
- **What it looks like**: after a run passes its checks, the result is
  photographed where the checks ran — a Flutter app's first frame or root
  widget, the screens the agent names, a web page in a headless browser, or
  any app with a screenshot command under a virtual display — and shown on
  the card, kept in DevDock's own screenshots folder. A run without a
  sandbox gets one started for the pictures alone. Never on your screen.
- **Nothing is lost**: a run that does not get through keeps its attempt on
  its branch, unpushed; open it in VS Code, finish it, or turn it into a pull
  request with one click. A check that already fails on the base branch is
  not held against the change. Each repository keeps its own agent state
  while another is open.
- **GitHub**: sign in via browser device flow or a personal access token,
  authenticated push/pull/fetch, list and **create pull requests**
  (with an AI-generated title and description written from **every commit on
  the branch** — its subjects, bodies, and full diff against the base — not
  from whatever happens to be staged).
- **Jira tickets from a list**: paste a list of work — or the findings of an
  AI review — and a model drafts a ticket for each item with the repository
  open to it, so each one names the file and function rather than restating
  the bullet. Every item is checked to have ended up in a ticket. Editable
  before anything is created. See [docs/jira-tickets.md](docs/jira-tickets.md).
- **Working the Jira backlog**: the unassigned tickets of a project, judged
  by a model with the repository open — this repository or not, which part,
  doable unattended or needs a person. Pick any; one agent per ticket runs
  in its own worktree, in parallel, with a Linux sandbox of its own (a Lima
  VM, an Apple container, or Docker; network on, root shell, installs kept), and
  ends as a draft pull request. Every agent is tracked live: state, elapsed,
  every tool call, the files it changed. Worktrees are removed when done;
  a ticket an agent starts is assigned to you, moved to the active sprint,
  and marked In Progress, and gets a comment with the pull request.
  Agents confer between rounds: a failed check or a give-up goes to a second
  agent for a diagnosis before the next attempt.
  See [docs/jira-tickets.md](docs/jira-tickets.md#working-the-backlog).
- **Prompts in a worktree of their own**: tick *In a fresh worktree* in the
  Agent tab and a prompt runs like a backlog ticket — its own branch and
  worktree, the checks, rounds with a reviewing agent, a draft pull request,
  and the worktree removed after — while your tree stays as it is. Every run
  is tracked as a card. See
  [docs/editor-and-agent.md](docs/editor-and-agent.md#in-a-fresh-worktree).
- **Stacked pull requests**: split one large change into a chain of branches,
  each PR targeting the branch below it so every reviewer sees one focused
  diff. Built on GitHub's own `gh stack` extension, so the stack is the one
  GitHub shows on each PR and the one `gh stack` shows in a terminal — restack
  after any branch changes, push and open every PR in one action, and sync
  after something merges. Needs `gh extension install github/gh-stack`.
  See [docs/stacked-prs.md](docs/stacked-prs.md).
- **Worktrees**: check a branch out in its own directory and open it in a
  second window — its own working tree, agent, and terminal — so two branches
  can be worked on at once, or a coding agent run on each of several branches
  at the same time. `devdock <dir>` opens the app on any checkout.
  See [docs/worktrees.md](docs/worktrees.md).

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

AI features (commit messages, PR text, conflict resolution, the
[code reviewer](docs/local-ci.md#ai-code-review), and the agents) need a
model. Set up either provider — DevDock does not ship one — or install the
`claude` command and pick **Claude Code agent** as the engine for coding,
the backlog, or review.

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
request), `ci`, `hook`, `worktree`, `backlog` (list, judge, and fix
tickets in parallel sandboxed worktrees), `version`, and `self-install`.
See [docs/cli.md](docs/cli.md).

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
  jira.rs      Jira Cloud: credentials, projects, issue creation, backlog, ADF
  backlog.rs   Fixing a task unattended: worktree, agent, rounds, advisor,
               reviewer, checks, draft PR; kept attempts
  sandbox.rs   A machine of the run's own: Lima, Apple container, Docker;
               toolchain recipes; Claude Code and MCP servers inside
  screenshots.rs  What the result looks like: Flutter, web, and command
               screenshots, taken where the checks ran
  ollama.rs    Commit-message generation client (library, reusable)
  review.rs    The AI review gate: config, prompts, findings, thresholds
  stack.rs     Stacked pull requests: a typed wrapper over `gh stack`
  agent/       Tool-use harness: read, edit, language server, check, shell,
               ask-the-developer and MCP tools; images in the prompt; the
               conflict resolver and the coding agent run on it
               claude_code.rs  Claude Code as an engine: permissions, MCP,
               questions with session resume, in the sandbox
               mcp.rs  A stdio MCP client for the harness
  lsp/         Language server client: JSON-RPC over stdio, one process per
               server, diagnostics and navigation for the editor and agent
  app/
    mod.rs     App state, config, background message pump
    theme.rs   Visual identity: colour tokens, type scale, bundled fonts
    markdown.rs  Markdown rendering for reviews, agent summaries, and .md files
    views.rs   Toolbar, sidebar, diff panel
    dialogs.rs Repo picker, GitHub, PRs, stacks, conflicts, settings
    worktrees.rs Worktrees: a branch per directory, a window per worktree
    backlog.rs Jira backlog: judged tickets, and the agents fixing them
    editor.rs  Code editor: buffers, highlighting, LSP interactions
    agent_tab.rs The coding agent's task panel and change review
    worker.rs  Background thread runner; messages routed per repository
build.rs     Stamps the build with its commit and date
tests/
  workflow.rs  End-to-end git workflow tests against throwaway repos
  stack.rs     Stacked PRs through gh stack (skipped when it is not installed)
  stack_live.rs  The same flow against real GitHub (ignored by default)
  agent.rs     Harness tests: sandbox limits, proposals, applied merges,
               and the coding agent against a real server and real checks
  lsp.rs       Language server client, against a real child process
```

The `git_manage` library (everything outside `app/`) has no UI dependencies
and can be reused to build other clients; `devdock backlog fix` is the same
pipeline the app runs, from a terminal.

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
