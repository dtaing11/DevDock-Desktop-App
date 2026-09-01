# Stacked pull requests

A stack turns one large change into a series of small ones. Instead of a single
branch off `main` carrying twelve commits, each self-contained piece gets its
own branch and its own pull request, and **each PR targets the branch below it**
rather than the trunk. Reviewers see one focused diff per PR, because everything
underneath is already in its base.

GitHub has no "stack" object. A stack *is* the chain of base branches — which is
why a stack made here is a perfectly ordinary set of pull requests to everyone
else, readable in the GitHub UI with no extension and no account anywhere.

Open it from **Stack** in the toolbar, **Stack…** in the pull request dialog, or
the command palette (`Stacked pull requests`).

## The model

```
  feat/report      PR #12  →  feat/validate
  feat/validate    PR #11  →  feat/parser
  feat/parser      PR #10  →  main
  main                        trunk
```

Each branch records what it is stacked on in the repository's git config:

```
branch.feat/validate.devdock-parent = feat/parser
branch.feat/validate.devdock-pr     = 11
```

That is the whole state. It is per-repository, survives everything git does to
your branches, and is invisible to anyone who does not use this app. Nothing is
stored in a service, a lockfile, or a commit message.

A stack is one straight chain. Branches can share a parent, and the view says so
when they do, but they are not folded into the chain: restacking a tree in one
action is a good way to lose track of what moved where.

## Building a stack

Start from any branch. An untracked branch is already a stack of one based on
the trunk, so there is no step that "starts" a stack.

**New branch on `<tip>`** creates the next branch on top of the highest one and
records the link. Commit into it as usual.

**Base…** on any entry changes what it is stacked on — moving a branch up or
down the chain, or re-pointing it at the trunk. The change is recorded
immediately; **Restack** is what applies it to the commits.

**Untrack** takes a branch out of the stack and leaves the branch itself alone.
Anything stacked on it moves down to what it was based on.

## Restack

When a branch low in the stack changes — a new commit, an amend, a rebase —
everything above it is out of date. **Restack** rebases each branch back on top
of its parent, bottom-up, so the chain is linear again.

The subtlety is *which* commits get replayed. Once the bottom branch is rebased,
its children point at commits whose parent no longer exists in the chain, and
`git merge-base` then finds the trunk as the common ancestor — so a naive rebase
replays the branch below's commits a second time. Every branch's tip is
therefore recorded before anything moves, and each rebase replays exactly
`<old parent tip>..<branch>`.

Restacking needs a clean working tree, and it refuses rather than stashing on
your behalf. If a rebase stops in conflict the restack stops there too, leaving
the rebase in progress and opening the conflict resolver: finishing or aborting
it is the same as any other conflict.

## Submit

**Submit stack** does, in order:

1. force-pushes every branch (with lease, so a branch someone else moved is
   refused rather than overwritten);
2. opens a pull request for any branch that has none, based on its parent, with
   a title and body drafted from the branch's own commits;
3. retargets the base of any PR whose parent has changed;
4. writes the stack map into every PR body, between markers, so re-submitting
   replaces it rather than piling up another copy.

The map looks like this, in each PR, marked where you are:

> **Stack** (top first)
>
> - #12 `feat/report`
> - #11 `feat/validate`  ⬅ **this PR**
> - #10 `feat/parser`
> - `main`

Submitting is refused while any branch is behind its parent. Restack first: a
stale branch's pull request shows the changes underneath it as its own, which is
exactly what a stack exists to avoid.

Like any other pull request, submitting goes through the AI review gate when
`[review] run = true`. The whole series is reviewed against the trunk.

## Sync (after something merges)

Merge from the bottom up. When the lowest PR lands, **Sync**:

1. fetches;
2. asks GitHub about each remembered pull request — a squash merge leaves no
   trace in local history, so GitHub's answer is the one that counts, with a
   patch-id comparison as the fallback for branches merged outside a PR;
3. drops merged branches out of the chain, re-parenting what sat on them (the
   branch above a merged one ends up based on the trunk);
4. rebases what is left onto its new parent.

Nothing is pushed and no branch is deleted: what the remote should look like
afterwards is a separate decision, made by submitting again. If GitHub cannot be
reached, the branch stays in the stack and the activity log says why — an
unreachable service must not silently drop work out of a stack.

## What it does not do

- **Deleting merged branches.** Sync takes them out of the stack; removing the
  branch is left to you.
- **Forks.** A pull request is opened with the branch name as its head, so the
  branch has to live in the repository the PR is opened against.
- **Splitting an existing branch into a stack.** Use **Split commits** (the AI
  split of the working tree) or `git rebase -i` first, then stack the branches.
