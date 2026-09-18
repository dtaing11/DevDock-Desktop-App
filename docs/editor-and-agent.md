# Editor, language servers, and the coding agent

DevDock has a code editor with language server support, and an agent that
uses the same tools you do. This guide covers all three.

- [The editor](#the-editor)
  - [Opening files](#opening-files)
  - [What the language server gives you](#what-the-language-server-gives-you)
  - [Keyboard](#keyboard)
- [Language servers](#language-servers)
  - [Built-in table](#built-in-table)
  - [Configuring your own](#configuring-your-own)
  - [When something is wrong](#when-something-is-wrong)
- [History, search, and undo](#history-search-and-undo)
- [AI history tools](#ai-history-tools)
- [The coding agent](#the-coding-agent)
  - [Two modes: propose, or let it iterate](#two-modes-propose-or-let-it-iterate)
  - [What it can and cannot do](#what-it-can-and-cannot-do)
  - [Reviewing its work](#reviewing-its-work)
  - [Following up](#following-up)
- [How this relates to the other AI features](#how-this-relates-to-the-other-ai-features)

---

## The editor

The **Editor** tab is a real editor, not a diff viewer: buffers, unsaved
state, syntax highlighting, and everything the language server for that file
contributes.

### Opening files

The sidebar is the **work tree**: directories expand, files open, and anything
with unsaved changes is marked. The file itself, its problems and its outline
are in the viewport — a 340pt column is no place to read code.

Five ways in, because you arrive from five directions:

| From | How |
|------|-----|
| The work tree | Click a file in the Editor sidebar |
| The Changes tab | Double-click a file |
| Anywhere | **Open…**, or Cmd/Ctrl+O — filters every tracked file |
| A diagnostic | Click it in the Problems panel |
| The code | F12 on a symbol, or Cmd/Ctrl+click |

Tabs show a `•` for unsaved changes and an error count in parentheses.
Closing a tab with unsaved work is refused rather than silently discarding
it. Files over 2 MB open read-only — highlighting a buffer that large stops
being interactive, and a git client is not where you edit a database dump.

### What the language server gives you

- **Diagnostics** — underlined in place, marked in the gutter, and listed in
  the **Problems** panel. Click one to jump to it. They update as you type,
  a moment after you stop.
- **Hover** — rest the pointer on a symbol for its type and documentation.
- **Go to definition** — F12, or Cmd/Ctrl+click. Opens the target file if it
  is not already open.
- **Find references** — Shift+F12. Results land in the **References** panel
  with the line of code, so you can usually decide without opening each one.
- **Outline** — the file's symbols, from the server, in the left panel.
  Toggle it with the **Outline** checkbox.
- **Completion** — Cmd/Ctrl+Space. Type to filter, arrows to move, Enter or
  Tab to insert, Esc to dismiss.
- **Format** — the **Format** button, or Cmd/Ctrl+Shift+F. Tick **on save**
  to format every save, which is the usual reason to want it.
- **Rename** — F2. The rename is worked out by the language server across the
  whole workspace, then shown as **proposed changes**: a diff per file that
  you accept individually. Nothing is written until you do.

### Keyboard

| Key | Action |
|-----|--------|
| Cmd/Ctrl+S | Save |
| Cmd/Ctrl+Shift+F | Format |
| Cmd/Ctrl+Space | Completion |
| F12 / Cmd/Ctrl+click | Go to definition |
| Shift+F12 | Find references |
| F2 | Rename symbol |
| Esc | Dismiss completion or hover |
| Cmd/Ctrl+O | Open file (rebindable in Settings) |

## Language servers

A server starts the first time you open a file it handles, and one server
covers every file of that language — that is what servers like
`rust-analyzer` expect, since they index the whole workspace.

The toolbar shows which server is attached and what it is doing
(`rust-analyzer: indexing`, then `ready`). Requests are answered on a
background thread, so a slow server never freezes the app.

### Built-in table

Matched by file extension, used if the command is on your `PATH`:

| Language | Command |
|----------|---------|
| Rust | `rust-analyzer` |
| Python | `pyright-langserver`, else `pylsp` |
| TypeScript / JavaScript | `typescript-language-server` |
| Go | `gopls` |
| C / C++ | `clangd` |
| Java | `jdtls` |
| Dart | `dart language-server` |
| Ruby | `solargraph` |
| PHP | `intelephense` |
| Lua | `lua-language-server` |
| JSON / YAML / HTML / CSS | the `vscode-*-language-server` family |
| Shell | `bash-language-server` |
| Zig | `zls` |
| Kotlin | `kotlin-language-server` |

Nothing is bundled: install the server you want the way you normally would.

### Configuring your own

Anything not in that table — or a different toolchain, a wrapper script, a
server with flags — goes in `.git-manage-ci.toml`, the same file local CI and
the review gate use:

```toml
[[lsp]]
extensions = ["rs"]
command = "rust-analyzer"
args = ["--log-file", "/tmp/ra.log"]
language_id = "rust"          # optional; defaults to the extension
```

```toml
# A server for a language with no built-in entry.
[[lsp]]
extensions = ["nim"]
command = "nimlangserver"
```

An override wins over the built-in table for those extensions. The file is
committed, so everyone on the project gets the same setup.

### When something is wrong

- **"no language server"** in the toolbar means nothing on `PATH` handles that
  extension. The error names what it looked for.
- **A server that will not start** reports its own stderr — for example
  `rust-analyzer said: error: Unknown binary 'rust-analyzer' in official
  toolchain`, which means the rustup shim is installed but the component is
  not (`rustup component add rust-analyzer`).
- **A wedged server**: **Restart servers** stops all of them; they start again
  on the next file and your open buffers are re-attached.

## History, search, and undo

**File history** — the **History** button on a selected file lists every
commit that touched it, following it through renames. A file's history
usually predates its current name, and stopping at the rename hides exactly
the commits you were looking for.

**Search** — the History tab searches four ways:

| Mode | What it finds |
|------|---------------|
| Message | words in commit messages |
| **Code** | commits that *added or removed* this text — git's pickaxe |
| Author | commits by a person |
| Path | commits touching a path |

Code search is the one worth knowing. It answers "when did this string appear,
and when did it go away?", which no amount of grepping the working tree can:
the answer is in history, not in the files.

**Undo** — the **Undo…** button lists where the branch has been, from the
reflog: merges, rebases, resets, commits. Going back to one is a **soft**
reset, so the changes from the undone commits stay staged in your working
tree. Nothing is deleted, and the state you left is itself in the reflog.

## AI history tools

Two tools that use the harness to clean up before a pull request. Both
propose; neither acts until you accept.

**Split** (Changes tab, when more than one file has changed). Groups the
working tree into the commits it should have been — a feature with its test
in one, an unrelated fix in another — and writes each message. Every changed
file must appear in exactly one group, which is checked before anything is
staged; a split that quietly left a file out would leave it uncommitted with
nobody the wiser. You can edit the messages and drop a group before applying.

**Tidy history** (History tab). Proposes how the branch's commits should be
folded and reworded: three commits called `wip`, `wip 2` and `fix typo` are
one commit with a real message. Then:

- Every sha must appear **exactly once**. A plan that drops a commit is lost
  work and a plan that repeats one applies it twice; both are refused before
  anything moves, and the model gets one retry with the reason.
- The rewrite replays the commits onto the base on a detached `HEAD`, and
  moves the branch only once every commit has landed. A conflict or a bad
  message leaves the branch exactly where it was.
- It refuses to run with uncommitted work, since replaying moves the working
  tree between commits.
- The old history stays in the reflog, so **Undo…** covers it.

## The coding agent

The **Agent** tab takes an instruction and works on the repository until it
can report back. It is the same harness the conflict resolver and the review
gate use, with a wider toolset.

Pick the provider and model next to the task box, exactly like commit
messages — this is the one worth pointing at your strongest model.

### What it is told

Before the task, the agent gets an overview of the repository — how many
files, which top-level directories, what languages, which project files exist
— and what is going on in it: the last few commits and any uncommitted
changes. That is what lets it search in the right place on its first turn
instead of its third.

If the repository has an `AGENTS.md` (or `CLAUDE.md`, or
`.github/copilot-instructions.md`), its contents are put in front of the
agent as the project's own instructions, ahead of the review guidance from
`.git-manage-ci.toml`. Only a tracked file counts.

### The plan

The agent writes down what it intends to do before it starts, and ticks each
step off as it finishes. That checklist is what the sidebar shows while it
works — a far better answer to "what is it doing" than a scrolling log of
tool calls, which is still there underneath if you want it.

### It is not allowed to skip the check

The harness knows what the agent verified, so it does not have to take the
summary's word for it. Three things get a model sent back, once each, with a
note saying exactly what it skipped — the log shows them as
`not finished yet: …`:

- finishing with edited files but no check run (in iterate mode, when the
  repository declares one) or no diagnostics on what it changed (when a
  language server is available);
- answering with the code in a code block instead of applying it — the
  classic small-model failure, and one every model shows now and then;
- handing the task back with a question for the developer, on a run where
  reading the file or running the check would have answered it. Nobody is
  there to answer mid-run; the note says which tool to use.

Long runs do not drown in their own reads: once the transcript passes a
budget, old tool output is replaced with a one-line note saying what it was.
The model can call the tool again if it still needs it.

### The engine: the built-in harness, or Claude Code

Next to the task box, **Engine** switches between **DevDock harness** and
**Claude Code agent**; the same switch sits beside the backlog fixer, the
reviewer, and each of those tasks in Settings. It is greyed out, with the
install command on hover, when the `claude` command is not on this machine.
The model picker lists the Claude Code aliases first for those tasks, too.
Picking Claude Code hands the task to Anthropic's own agent, run headless in the
working tree (`claude -p … --output-format stream-json`), instead of this
app's tool-use loop. Everything else is the same: its tool calls stream into
the same log, its `TodoWrite` list is the plan the sidebar shows, and its
changes end in the same Keep/Revert review.

Two things follow from what Claude Code is:

- It **writes to disk as it works**, so it needs **Let it iterate** on. The
  changes are found afterwards by comparing the tree with a snapshot taken
  before the run, which is what makes Revert exact.
- Its `Bash` tool is **open**, less the same things the built-in harness
  refuses: git commands that commit, push, or rewrite history (`commit`,
  `push`, `reset`, `checkout`, `rebase`, `stash`, …), `sudo`, and the web
  tools. Reading git (`status`, `diff`, `log`, `show`) is fine. The system
  prompt names the repository's toolchain (`flutter` and `dart` for a
  Flutter app, `cargo` for Rust, `npm` for Node…) and says a denied command
  will stay denied, so it does not spend turns retrying one. A denial shows
  in the log as `tool error: … requires approval`. The CLI also refuses
  `sleep` and log-polling loops; the prompt tells the agent to run every
  command to completion in the foreground instead, so a refusal like
  `Blocked: sleep 60 followed by …` in a log means the model ignored that
  once, not that anything is broken. Headless runs load the MCP servers
  the repository declares in its `.mcp.json` with all their tools allowed
  up front; a plugin's or a user-level server that DevDock could not list
  is allowed the first time the model is refused one of its tools, and
  the session is resumed — one refusal per server, not one per call. With
  a sandbox, Claude Code and its servers run inside it. The reviewer, run
  with reading tools, may also run the repository's checks and read-only
  commands, and may read dependency sources (cargo's registry, pub's
  cache, the Flutter SDK), but not edit.

The model under it is a Claude Code alias (`default`, `sonnet`, `opus`,
`haiku` — the latest of each family) or any model id your account has,
version and all (`claude-fable-5-1`, `claude-opus-5`); the picker lists
both. The same choice is available for the backlog fixer; judging the
backlog is a read-only harness run, so that still uses a model.

**How to tell which harness ran.** Every run's log opens with an `engine:`
line — `engine: Claude Code agent` or `engine: DevDock harness · Claude
(claude-sonnet-5)` — and
the strip above the summary repeats it next to the turn count. A backlog
card and its draft pull request say the same. While Claude Code is
running, `claude -p …` is a child process of DevDock, which `ps` will show.

### A third engine: OpenCode

**OpenCode agent** appears beside the other two when the `opencode` command
is installed (`curl -fsSL https://opencode.ai/install | bash`). It is an
open-source, provider-agnostic agent: pick any model it knows, in
`provider/model` form — its own `opencode/…` models are free and need no
sign-in, and `anthropic/…` models use DevDock's Claude sign-in, handed over
as an access token for the run. DevDock runs it headless with the same
rules as Claude Code: a full shell less git history and the web, the
repository's `.mcp.json` servers in its config, images attached as files, a
question answered by resuming the session, and inside the sandbox when
there is one. It needs "Let it iterate", like Claude Code.

### Two modes: propose, or let it iterate

**Propose** (default). The agent reads and edits, but its edits are held in
memory. Nothing on disk changes until you accept a file. The cost is real:
it cannot compile or test what it wrote, and it is told to say so in its
summary rather than claim work it could not check.

**Let it iterate** (the checkbox). Edits are written to your working tree as
it makes them, which is what allows the other half of the loop:

- `diagnostics` — it asks the language server what it just broke, and fixes
  it before moving on.
- `run_command` — a shell in the repository: build it, run one test, format,
  install a dependency. Git commands that change history are refused.
- `run_check` — it runs the checks **your repository declares** in
  `.git-manage-ci.toml`, and sees the output. A repository that declares
  none gets the checks its toolchain implies — `flutter analyze` and
  `flutter test`, `cargo build` and `cargo test`, `npm test`, `pytest`,
  `go vet` and `go test` — and the log says they were inferred. Declare
  your own to be exact.

You still review every change at the end; the buttons become **Keep** and
**Revert**, and reverting restores exactly what was there before the run,
including deleting files it created.

Whatever the mode, the engine, or the language, the agent is held to one
standard, and the reviewing agent of a worktree run enforces it: reusable
over ad hoc, one responsibility per function and type with behaviour kept
with its data, clear names, no magic numbers or dead code, the repository's
own conventions, and a test for what changed.

Use propose for anything you want to inspect first. Use iterate when the task
has a definition of done the machine can check — "make the tests pass",
"fix the build", "add a flag and cover it with a test".

### In a fresh worktree

**In a fresh worktree** (the checkbox under the task box) sends the prompt
the way [the backlog fixer](jira-tickets.md#working-the-backlog) sends a
ticket, and leaves this window's tree alone:

1. a branch — `agent/<the prompt's first line>`, or the name you type — and a
   worktree for it, made from the default branch;
2. the chosen engine, live, in that worktree, with the repository's checks
   (declared, or inferred from the toolchain);
3. the checks run again by DevDock, not on the agent's word; a failure goes
   back to the agent for up to **rounds** attempts (up to ten), with the
   output and a second agent's diagnosis of it — the code-review model, or
   the same engine in a fresh session, reads the code and says what to do.
   An agent that changed nothing and said it needs a person is sent back the
   same way, unless the second agent agrees. A round that leaves the tree as
   the last one did ends the run;
4. optionally a **second agent** — the code-review model — reading the prompt
   and the diff, and sending it back with feedback until it approves;
5. a commit, a push, and a **draft pull request**;
6. the worktree removed. A run that changed nothing, or never passed, also
   deletes its branch — nothing is left but the log.

Every run is a card in the viewport, keyed by branch: queued, running, done or
failed, with elapsed time, everything it did as it did it, and at the end the
pull request and the files it changed. Several can run at once — send one
prompt, then the next — and the task box is free as soon as one starts. The
cards stay until **Clear finished**.

**Run in a sandbox** gives the run a Linux machine of its own — a Lima VM,
an Apple container, or Docker, whichever is installed — with the network on
and a root shell, where the agent installs what it needs and builds and
tests, and where DevDock's verification runs too. What it installs stays for
the next run. See [the sandbox](jira-tickets.md#the-sandbox).

It needs GitHub sign-in, because the outcome is a pull request. A prompt that
needs a decision from you is not a fit: the agent is told nobody can answer,
and to change nothing rather than guess.

### Every run, in one tab

The **Runs** tab lists every agent run the window knows of: the runs in
the repository on screen and in every repository kept aside — worktree
runs, backlog tickets, and the run in each tree. Its label counts what is
running everywhere. The sidebar filters — all, running, waiting on you,
failed, done — and shows each repository's counts; the viewport groups the
cards by repository, the one on screen first, with a Show button for the
others.

A card is the same one the Agent tab and the backlog show: it unfolds to
its log; one with a question takes your answer there; a failed one offers
**Open in VS Code** for its kept attempt and **Open as PR** to finish it
as a pull request — for any repository, without switching to it. The run
in a tree has a card too, with a button to its Agent tab. **Clear
finished everywhere** drops the done and failed cards in every
repository; kept attempts stay on their branches.

### MCP tools

A repository that declares MCP servers in its `.mcp.json` gives them to
both engines. The built-in harness starts each stdio server when a run
begins — inside the sandbox when there is one, so a server that touches
files or runs commands touches the same tree the checks do — and offers
its tools to the model as `mcp__<server>__<tool>`, next to its own; they
are stopped when the run ends. Claude Code is given the same file and every
declared server's tools up front; a plugin's or user-level server it finds
on its own is allowed the first time one of its tools is refused. Servers
reached by URL rather than a command are Claude Code's only: the harness
speaks stdio.

### Images in the prompt

Some things words describe badly: a screenshot of the bug, a mockup of what
the screen should become, a photo of a whiteboard. **Attach image…** under
the task box adds PNG, JPEG, GIF or WebP files, and files dropped anywhere
on the window while the Agent tab is up are attached too; thumbnails show
what is attached, with a way to take one off. The images go with the task —
in this tree or in a fresh worktree — and are gone from the box once it
starts. A Claude model sees them directly; an Ollama model does if it is a
vision model; Claude Code reads them as files DevDock puts in the worktree
for the run and removes after, never part of the change.

### When it is not sure, it asks

An agent that hits something it cannot decide — a product choice, two
reasonable readings of the task, a value nobody wrote down — asks you rather
than guessing. The built-in harness has an `ask_developer` tool; Claude
Code ends its reply with a `QUESTION:` line and is resumed in the same
session with the answer. Either way a box appears on the run's card (or at
the top of the Agent panel for a run in this tree) with the question and a
place to type. **Answer** sends it; **Let it decide** sends nothing, and
the agent proceeds on its best assumption and says what it assumed. An
unanswered question times out after half an hour the same way. Runs from
the command line are unattended and have no such line: the agent decides.

### What it can and cannot do

| Tool | What it does |
|------|--------------|
| `list_files`, `read_file`, `search` | Read every file **git tracks**; search is literal or regex, with context lines |
| `write_file`, `edit_file`, `replace_lines` | Propose or write changes. An edit whose snippet differs from the file only in whitespace, or is over 90% similar to exactly one region, still applies and says so; one that does not match quotes the region it was probably aiming at, numbered. `replace_lines` edits by the line numbers `read_file` showed, for text that is awkward to reproduce (escapes, tabs). A no-op edit is reported as one, not as an error |
| `show_changes` | The diff of everything it has changed so far, as you will see it |
| `diagnostics` | Errors and warnings from the language server |
| `definition`, `references`, `find_symbol` | Navigate like you do |
| `run_check` | Run one of your declared checks |
| `run_command` | Run a shell command in the repository — the toolchain, one test, a formatter, a package manager. Live runs only |

Three limits are structural, not prompt instructions:

- **Tracked files only.** Ignored files — `.env`, credentials, build output —
  are invisible to it and never reach a model provider.
- **Inside the repository.** Absolute paths, `..`, and symlinks out are
  refused, as is anything under `.git/`.
- **No git history from inside a run.** `run_command` is a real shell, on a
  live tree only (it would see the old code otherwise), with a timeout and
  capped output — but `git commit`, `push`, `reset`, `checkout`, `rebase`,
  `stash` and the like are refused, as is `sudo`. You commit what you keep;
  a worktree run commits for itself when it is done. In a worktree run with
  a sandbox, every command runs inside it.

Budgets bound each run (turns, tool calls, bytes read, check runs). When one
runs out the agent is asked to finish with what it has, and the panel says
the run was cut short.

When it is done, the strip above the summary shows how many turns it took and
what it cost in tokens — what the model read fresh, what came from the prompt
cache (the system prompt, the tools, and the transcript so far are cached
between turns, so most of a long run is served at a fraction of the price),
and what it wrote.

### Measuring it

`examples/agent_eval.rs` runs the agent on suites of checkable tasks in
Python and Rust — implement from tests, fix a bug, a feature across files,
a rename, a question over a repository, an exact edit in an awkward file
(`EVAL_SET` unset); regressions only the whole suite reveals, a trait change
across a crate, a 150-file repository, a spec in the docs, a test-tampering
trap (`EVAL_SET=hard`); a cache with interacting rules, three bugs at once, a
lifetime refactor, a performance fix under a timer, ten callers, a config
migration, a CRLF file (`EVAL_SET=brutal`); and twenty-five files that all
have to change consistently (`EVAL_SET=marathon`). Each run is graded by
rerunning the repository's check afterwards, and by checking that the test
files are byte-for-byte unchanged.

```sh
cargo run --release --example agent_eval                      # the basic suite, Haiku
EVAL_SET=hard EVAL_REPEATS=2 cargo run --release --example agent_eval
EVAL_PROVIDER=ollama EVAL_MODEL=qwen2.5-coder:7b cargo run --release --example agent_eval
```

It prints a row per run — pass/fail, turns, tool calls, tool errors, bytes
sent, the size of the diff, seconds, tokens — and writes `agent-eval.json`,
so two versions of the agent can be compared on the same tasks.

### Reviewing its work

The changes appear as a list with a diff per file. Tick the ones you want and
apply (or revert, in iterate mode). **Open in editor** puts the selected file
in front of you with its diagnostics, which is often the faster way to judge
a change.

If a file it wrote is open in the editor, the buffer follows the file —
unless you have unsaved edits in it, in which case your version is kept and
the panel says so.

### Following up

The tab is a conversation. Each task and the summary it produced stay listed,
and the next task is sent with them, so "now do the same for the other
module" works. **New session** forgets the thread.

Starting a new task while changes are unreviewed is flagged: two runs' edits
in one list would be impossible to untangle.

## How this relates to the other AI features

| Feature | Tools | Writes |
|---------|-------|--------|
| Commit message / PR text | none — reads a diff | no |
| [Code review gate](local-ci.md#ai-code-review) | read-only, whole repo | no |
| [Conflict resolver](../README.md#features) | read + edit, whole worktree | proposals only |
| Coding agent | read, edit, language server, checks | proposals, or live with revert |

They share one harness, one set of sandbox rules, and one confirmation model:
**a model proposes, you decide.**
