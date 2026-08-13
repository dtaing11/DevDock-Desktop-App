# DevDock CLI

The `devdock` binary is both the GUI and a command-line tool. With no
arguments it launches the app; with a subcommand it runs headlessly, so
it works in terminals, scripts, and git hooks.

```
devdock              # launch the GUI
devdock <command>    # run headlessly
```

- [Commands](#commands)
  - [status](#status)
  - [log](#log)
  - [branches](#branches)
  - [stash](#stash)
  - [commit](#commit)
  - [push](#push)
  - [pr](#pr)
  - [resolve](#resolve)
  - [ci](#ci)
  - [ci init](#ci-init)
  - [hook](#hook)
- [The CI gate](#the-ci-gate)
- [Auth and configuration](#auth-and-configuration)
- [Exit codes](#exit-codes)
- [Scripting recipes](#scripting-recipes)

## Commands

### status

Working-tree summary: branch, sync state, and changed files.

```
$ devdock status
branch: feature/git-graph (ahead 2, behind 0)
  + src/cli.rs
  * src/main.rs
2 changed file(s)  (+ staged, * unstaged, ! conflict)
```

### log

Last N commits (default 10).

```
$ devdock log 3
2d8ab5f  fix: graph hover popup on tooltip layer  dinataing · 2026-08-13
5631283  feat: graph as resizable side panel      dinataing · 2026-08-13
039ba34  fix: GitHub sign-in consistency          dinataing · 2026-08-13
```

### branches

Local branches (current marked `*`) followed by remote branches.

```
$ devdock branches
* feature/git-graph
  main
  origin/main (remote)
```

### stash

```
devdock stash list          # stashes with their origin branch
devdock stash save [MSG]    # stash all changes, including untracked
devdock stash pop [INDEX]   # apply and drop (default: newest, index 0)
```

```
$ devdock stash list
  [0] (main) WIP on main: 1a2b3c fix header
  [1] (feature/x) auto-stash before switching to main
```

### commit

Stages **everything** and commits.

```
devdock commit -m "fix: handle empty input"
devdock commit --ai        # AI-generated, with interactive review
```

`--ai` generates a message and shows it for review before committing:

```
$ devdock commit --ai
--- generated commit message ---
fix: debounce search input to avoid redundant queries

- Adds a 300ms debounce to the search box
--- end ---
[a]ccept / [r]egenerate / [e]dit manually / [q]uit? 
```

- **a** (or Enter): commit with the shown message
- **r**: ask the model for a new suggestion
- **e**: type your own title, then body lines (finish with a `.` line)
- **q**: abort without committing

The model is whatever you selected in the GUI (Ollama or Claude);
sign-in/config happens there and the CLI reuses it.

### push

Pushes the current branch. Publishes (sets upstream) automatically when
the branch has no upstream yet. Uses your GitHub sign-in for
github.com HTTPS remotes.

```
devdock push               # gated by local CI when configured
devdock push --force       # force-with-lease (after amend/rebase)
devdock push --no-verify   # skip the local CI gate
```

### pr

Runs the CI gate, pushes the branch, then opens a pull request into the
default branch (`main`/`master`).

```
devdock pr -t "Add search" -b "Implements fuzzy search over titles"
devdock pr --ai            # AI-generated, same review flow as commit --ai
devdock pr -t "Fix" --no-verify
```

`--ai` shows the generated title/description with the same
accept / regenerate / edit / quit prompt before anything is pushed.

```
$ devdock pr --ai
devdock: running local checks…
devdock ci: lint ... PASS (0.5s)
devdock ci: tests ... PASS (2.1s)
PR #7 created: https://github.com/you/repo/pull/7
```

Requires GitHub sign-in (done once in the GUI) and a github.com remote.
Refuses to open a PR from `main` onto itself.

### resolve

Interactive merge-conflict resolver. Walks each conflicted file and lets
you keep ours, keep theirs, or ask the configured AI model to propose a
merge from the base/ours/theirs versions.

```
$ devdock resolve
── resolve ─────────────────────────── 2 conflicted file(s)

[conflict] src/lib.rs
[a]i merge [o]urs [t]heirs [s]kip [q]uit ? a
asking the AI to merge…

── AI proposed merge for src/lib.rs ──
  ...full merged file...
── end ──
nothing is applied until you accept
[a]ccept [d]ecline ? a
resolved src/lib.rs
```

AI proposals are never applied silently: the full merged content is
printed and must be explicitly accepted, mirroring the GUI's
review-then-accept flow. Custom per-repo conflict instructions from the
AI Prompts dialog apply here too. When every file is resolved, the
command offers to run the merge/rebase continue step for you.

### ci

Runs every job in `.git-manage-ci.toml` and prints per-job results.
Exit code 0 when all pass. This is what the pre-push hook calls.

```
$ devdock ci
devdock ci: lint ... PASS (0.5s)
devdock ci: tests ... FAIL (2.4s)
    test result: FAILED. 1 failed
```

See [local-ci.md](local-ci.md) for job configuration (Docker
environments, secrets, `[on_push]`).

### ci init

Creates `.git-manage-ci.toml`. Plain `ci init` writes the commented
starter template. `ci init --ai` scans the repository (file listing plus
manifests like Cargo.toml, package.json, go.mod, Makefile) and asks the
configured AI model to draft jobs tailored to the project.

```
$ devdock ci init --ai
scanning the repository…
asking the AI to draft the config…

── AI proposed .git-manage-ci.toml ──
  [[job]]
  name = "tests"
  commands = ["cargo test"]
  ...
── end ──
nothing is written until you accept
[a]ccept [e]dit in $EDITOR [q]uit ? a
saved .git-manage-ci.toml
```

The draft is validated as TOML before you ever see it, and again after
`[e]dit`. Nothing is written until you accept; quitting discards it.
An existing config is only overwritten after the same confirmation.

### hook

Manages the git pre-push hook that runs `devdock ci` before every
`git push` from any terminal.

```
devdock hook install
devdock hook remove
devdock hook status
```

The installer refuses to overwrite a pre-existing hook it didn't create.

## The CI gate

`push` and `pr` honor the repository's `[on_push]` config:

```toml
# .git-manage-ci.toml
[on_push]
run = true               # run all jobs before push/pr
block_on_failure = true  # failing jobs abort the operation
```

| Setting | Behavior |
|---------|----------|
| `run = false` (or no config) | no gate, push/pr proceed directly |
| `run = true, block_on_failure = true` | failing checks **abort** |
| `run = true, block_on_failure = false` | failing checks warn, continue |
| `--no-verify` flag | skips the gate for this invocation |

The same semantics apply in three places: the GUI push button, the
`devdock push`/`pr` commands, and the git pre-push hook.

## Auth and configuration

The CLI shares all state with the GUI:

| What | Where | Set up via |
|------|-------|-----------|
| GitHub token | encrypted `auth.json` | GUI: GitHub button (device flow or PAT) |
| Claude credentials | encrypted `claude.json` | GUI: Settings |
| AI model selection | `config.json` | GUI: picker next to the AI buttons |
| CI jobs | `.git-manage-ci.toml` in the repo | any editor |
| CI secrets | `.git-manage-ci.secrets` (gitignored) | any editor |

Config lives in `~/.config/devdock` (Linux) or
`~/Library/Application Support/devdock` (macOS). All files are
AES-256-GCM encrypted at rest.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success / all checks passed |
| 1 | operation failed (git error, checks failed, network) |
| 2 | usage error (unknown command, missing argument) |

## Scripting recipes

**Commit and ship in one line:**

```sh
devdock commit --ai && devdock push
```

**Feature branch to PR:**

```sh
git checkout -b feature/thing
# ... edit files ...
devdock commit --ai
devdock pr --ai
```

**CI in a cron/watcher:**

```sh
devdock ci || notify-send "DevDock" "checks failing on $(pwd)"
```

**Guard a deploy script:**

```sh
devdock ci && ./deploy.sh
```
