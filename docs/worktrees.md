# Worktrees

A worktree is the same repository checked out a second time, in its own
directory, on its own branch. Two branches can then be worked on at once
without stashing anything — a review in one window while a feature carries on
in another — and, the reason this exists in DevDock, a **coding agent can run
on each of several branches at the same time**, each in its own window with
its own working tree, so none of them trips over another's half-written files.

Everything is plain `git worktree`. A worktree made here is an ordinary one
that `git worktree list` shows in a terminal, and the other way round.

Open it from **Worktrees…** in the branch menu, or the command palette
(`Worktrees`).

## One window per worktree

A DevDock window is one repository: one working tree, one agent, one
terminal. So a second worktree gets a second window — a second `devdock`
process, started on the worktree's path:

```sh
devdock ~/code/app-feat-search     # open the app on that directory
```

That is what **New window** does, and what **Create worktree** does by default.
The processes share the config file and nothing else. Closing one does not
affect the others.

## Creating one

**New worktree** in the dialog takes:

- **Branch.** An existing branch is checked out into the new directory; git
  refuses if it is already checked out somewhere, which is the right answer —
  two directories on one branch is how commits get lost. A name that does not
  exist yet is created.
- **Start from**, for a new branch: any commit or branch; empty means the
  current `HEAD`.
- **Directory.** Empty means next to the main worktree, named after the
  repository and the branch: `~/code/app` on `feat/search` gives
  `~/code/app-feat-search`.
- **Open in a new window**, ticked by default. Unticked, this window switches
  to the new worktree instead.

## Running agents on several branches

Create a worktree per branch, each in a new window, and give each window's
**Agent** tab its task. Every agent reads and writes only its own directory;
checks it runs (`cargo test`, and so on) run there too, so a failing build in
one branch is not visible from another. Commit and push from each window as
usual.

## Removing one

**Remove** deletes the worktree's directory and is refused if it has
uncommitted changes. **Remove, discarding** deletes it regardless. Either way
the branch is kept — remove it from the branch menu if it is done with.
**Prune missing** forgets worktrees whose directories were deleted by hand.

The main worktree, and the one this window is in, cannot be removed from here.

## From the terminal

```sh
devdock worktree                              # list
devdock worktree add feat/search              # existing branch, or create it from HEAD
devdock worktree add feat/search ~/code/x     # at a chosen directory
devdock worktree add feat/search --from main  # a new branch from main
devdock worktree remove ~/code/app-feat-search [--force]
devdock ~/code/app-feat-search                # open the app there
```

## Checking it yourself

`tests/workflow.rs` covers listing, adding an existing and a new branch, the
refusal to check out a branch twice, removing, and the default directory name.
An app test creates a worktree through the dialog and checks the window
switched to it.
