# Stacked pull requests

A stack turns one large change into a series of small ones. Instead of a single
branch off `main` carrying twelve commits, each self-contained piece gets its
own branch and its own pull request, and **each PR targets the branch below it**
rather than the trunk. Reviewers see one focused diff per PR, because everything
underneath is already in its base.

DevDock drives GitHub's own tooling for this — the
[`gh stack`](https://github.com/github/gh-stack) extension to the GitHub CLI —
rather than keeping a chain of its own. The stack you build here is the same
stack `gh stack view` shows in a terminal, and the same **Stack** GitHub shows
on each pull request; anyone on the team can pick it up with `gh stack checkout`
whether or not they use this app.

Open it from **Stack** in the toolbar, **Stack…** in the pull request dialog, or
the command palette (`Stacked pull requests`).

## Setup

The GitHub CLI and the extension have to be on the machine:

```sh
brew install gh                              # or your package manager
gh extension install github/gh-stack
```

If either is missing the stack dialog says so, with that command. No
`gh auth login` is needed: DevDock hands `gh` the token it is already signed in
with, for the API and for the pushes and fetches `gh` runs underneath.

## The model

```
  feat/report      PR #12  →  feat/validate
  feat/validate    PR #11  →  feat/parser
  feat/parser      PR #10  →  main
  main                        trunk
```

`gh stack` records the chain in `.git/gh-stack`, per repository and never
committed. DevDock reads it back through `gh stack view --json` and adds what
the view needs: the commits each branch carries on top of its parent, and
whether every one of them is already in the trunk.

A stack is one straight chain. A branch has one parent; a merged branch stays in
the chain, marked, so what sat on it is still known to sit on it — its pull
request is retargeted to the trunk by GitHub and the view shows it as based
there.

## Building a stack

Start from any branch. An untracked branch is not a stack yet, and the dialog
says so:

- **Track this branch** makes it a stack of one, based on the trunk
  (`gh stack init`).
- **New branch on `<tip>`** creates the next branch on top of the highest one,
  checks it out, and records the link (`gh stack add`). On an untracked branch
  it tracks that branch and the new one together. Commit into it as usual.
- **Import DevDock's stack** appears when an earlier version of DevDock
  recorded a chain for this branch in git config. It hands the chain to
  `gh stack` and forgets the old record.

Restructuring — reordering, dropping a branch from the middle, folding two
together, inserting, renaming — is `gh stack modify`, which is a terminal UI.
**Modify…** opens it in DevDock's terminal panel. **Untrack stack** forgets
the stack locally (`gh stack unstack --local`) and leaves the branches alone.

## Restack

When a branch low in the stack changes — a new commit, an amend, a rebase —
everything above it is out of date, and the view marks it **[behind]**.
**Restack** runs `gh stack rebase`: it fetches the trunk, fast-forwards the
local one if it is behind, and rebases each branch back on top of its parent,
bottom-up. Without a remote it rebases the branches onto each other only.

`gh stack` remembers where each branch left its parent, so a rebase replays
exactly that branch's own commits — never the branch below's a second time,
even after that branch has been rewritten.

Restacking needs a clean working tree (untracked files are fine), and it refuses
rather than stashing on your behalf. If a rebase stops in conflict the restack
stops there too, leaving the rebase in progress and the conflict resolver ready
for it. Resolve and stage the files — in the resolver or by hand — then
**Continue restack** carries on up the stack, or **Abort restack** puts every
branch back where it was.

## Submit

**Submit stack** is `gh stack submit --auto`. It:

1. force-pushes every branch (with lease, so a branch someone else moved is
   refused rather than overwritten);
2. opens a pull request for any branch that has none, based on its parent, with
   a title and body from the branch's commits;
3. retargets the base of any pull request whose parent has changed;
4. creates or updates the stack on GitHub, so each pull request shows where it
   sits.

New pull requests are drafts unless **Ready for review** is ticked, which also
marks existing drafts in the stack ready.

Submitting is refused while any branch is behind its parent. Restack first: a
stale branch's pull request shows the changes underneath it as its own, which is
exactly what a stack exists to avoid.

Like any other pull request, submitting goes through the AI review gate when
`[review] run = true`. The whole series is reviewed against the trunk.

## Sync

Merge from the bottom up. When the lowest PR lands, **Sync** runs
`gh stack sync`:

1. fetches, and fast-forwards the trunk to match the remote;
2. reads each pull request's state back — a squash merge leaves no trace in
   local history, so GitHub's answer is the one that counts;
3. rebases what is left onto its updated parent, the branch above a merged one
   ending up on the trunk;
4. pushes every branch (atomically, with lease) and links the open pull
   requests into the stack on GitHub.

Nothing is deleted: a merged branch stays in the list, marked **[merged]**,
and pruning it is `gh stack sync --prune` in a terminal. A conflict during sync
restores every branch and reports it — resolving conflicts is what **Restack**
is for.

## Push all

**Push all** is `gh stack push`: every active branch force-pushed with lease,
nothing opened.

## Checking it yourself

`tests/stack.rs` covers the offline half against a bare repository on disk:
reading the chain, growing it, restacking (including the case where a rebase
would otherwise replay a branch's commits twice), a conflict continued and
abandoned, pushing, untracking, and the import of a chain the previous
implementation recorded. The tests need `gh` and the extension installed and
say so when they are not.

The GitHub half is exercised against the real API by an ignored test:

```sh
cargo test --test stack_live -- --ignored --nocapture
```

It creates a private scratch repository, submits a three-branch stack, checks
each pull request targets the branch below it and shows only its own files,
squash-merges the bottom one, syncs, and checks that the branch above it was
rebased onto the trunk with only its own commit and its pull request
retargeted. The repository is left behind for you to delete.

## What it does not do

- **Deleting merged branches.** `gh stack sync --prune`, in a terminal.
- **Restructuring in the dialog.** That is `gh stack modify`, a terminal UI,
  reachable from **Modify…**.
- **Forks.** A pull request is opened with the branch name as its head, so the
  branch has to live in the repository the PR is opened against.
- **Splitting an existing branch into a stack.** Use **Split commits** (the AI
  split of the working tree) or `git rebase -i` first, then adopt the branches
  with `gh stack init a b c`.
