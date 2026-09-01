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

### The plan

The agent writes down what it intends to do before it starts, and ticks each
step off as it finishes. That checklist is what the sidebar shows while it
works — a far better answer to "what is it doing" than a scrolling log of
tool calls, which is still there underneath if you want it.

### Two modes: propose, or let it iterate

**Propose** (default). The agent reads and edits, but its edits are held in
memory. Nothing on disk changes until you accept a file. The cost is real:
it cannot compile or test what it wrote, and it is told to say so in its
summary rather than claim work it could not check.

**Let it iterate** (the checkbox). Edits are written to your working tree as
it makes them, which is what allows the other half of the loop:

- `diagnostics` — it asks the language server what it just broke, and fixes
  it before moving on.
- `run_check` — it runs the checks **your repository already declares** in
  `.git-manage-ci.toml`, and sees the output.

You still review every change at the end; the buttons become **Keep** and
**Revert**, and reverting restores exactly what was there before the run,
including deleting files it created.

Use propose for anything you want to inspect first. Use iterate when the task
has a definition of done the machine can check — "make the tests pass",
"fix the build", "add a flag and cover it with a test".

### What it can and cannot do

| Tool | What it does |
|------|--------------|
| `list_files`, `read_file`, `search` | Read every file **git tracks** |
| `write_file`, `edit_file` | Propose or write changes |
| `diagnostics` | Errors and warnings from the language server |
| `definition`, `references`, `find_symbol` | Navigate like you do |
| `run_check` | Run one of your declared checks |

Three limits are structural, not prompt instructions:

- **Tracked files only.** Ignored files — `.env`, credentials, build output —
  are invisible to it and never reach a model provider.
- **Inside the repository.** Absolute paths, `..`, and symlinks out are
  refused, as is anything under `.git/`.
- **No arbitrary commands.** `run_check` takes the *name* of a job from your
  config. There is no shell to inject into, and a run with no checks
  configured has no way to execute anything.

Budgets bound each run (turns, tool calls, bytes read, check runs). When one
runs out the agent is asked to finish with what it has, and the panel says
the run was cut short.

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
