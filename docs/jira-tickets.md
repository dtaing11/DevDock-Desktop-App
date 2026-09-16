# Writing Jira tickets from a list

A list of work — a planning document's bullets, a stand-up's actions, the
findings of a review — becomes a set of Jira tickets that name the file, the
function, and what is currently there. The model reads the repository while it
drafts, which is the only reason to have a model do this at all: a ticket that
restates the bullet in more words could have been written by a copy-paste.

Nothing reaches Jira until you say so. Drafts are editable in place.

Open it from the command palette (`Write Jira tickets`), or from the AI review
gate's **File as tickets** button, which fills the list with the findings.

## Connecting

Jira Cloud, with an email address and an API token from
[id.atlassian.com](https://id.atlassian.com/manage-profile/security/api-tokens).
The site can be pasted in any of the forms people have to hand — `acme`,
`acme.atlassian.net`, `https://acme.atlassian.net`, or the URL of a board.

The token is stored encrypted at rest like every other credential here, and
only after Jira has accepted it: a typo is not written to disk to fail again
on the next launch. **Sign out** deletes it.

## The list

One item per line. Bullets (`-`, `*`, `+`, `•`), numbers (`1.`, `2)`), and
checkboxes (`- [ ]`, `- [x]`) are understood and stripped; blank lines and a
line that is only a bullet are not items.

Every item ends up in a ticket. That is checked, not hoped for: the list is
numbered, each draft says which items it covers, and a draft that leaves an
item out is rejected and asked for again with the reason. One ticket may cover
several items when they are one piece of work, and one item may become several
when it is plainly several.

## The drafts

Each draft shows the item it came from, so coverage is visible without
counting. Summary, type and description are editable before creating —
the model will get a summary slightly wrong, and retyping it in the browser
afterwards defeats the point.

Descriptions are written in Markdown and converted to Atlassian Document
Format on the way out, because v3 of the API does not take text. Paragraphs,
headings, bullet and ordered lists, fenced code, inline code and bold all
survive; anything the converter does not model is carried through as text
rather than dropped.

Labels are lower-cased and capped at three. A label with a space in it is
hyphenated rather than sent, since Jira rejects it — and "needs design" is
exactly what a model writes when asked for labels.

## Creating

Pick a project; the issue types come from that project, so the model is told
what it actually has rather than guessing "Task". Sub-task types are not
offered, since they cannot be created without a parent.

**Create** files the ticked drafts. Each becomes a link to the issue. A draft
that fails says why on the card and stays ticked; the ones that succeeded are
not created again.

## From the command line

```sh
devdock tickets plan.md                          # draft and print
devdock tickets plan.md --create --project DEV   # and file them
cat notes.txt | devdock tickets                  # or from stdin
```

Printing by default and creating on request: a command that files twenty
tickets because someone piped the wrong file is not one people run twice.
`--create` needs `--project` and a connection made once from the app.

The command has no model picker, so it uses the model configured for the
closest task — tickets, then the coding agent, the reviewer, pull request
text, commit messages — rather than failing because one more per-task
selection was unset.

## Working the backlog

The other direction: instead of putting work into Jira, take it out. **Work
the backlog…** in the ticket dialog (or the command palette, `Work the Jira
backlog`) lists a project's unassigned, unresolved tickets and asks a model
to judge each one with the repository open to it:

- **in scope** — is the ticket about code in this repository at all, and
  which part of it (a monorepo has several);
- **autonomous** — could an agent finish it without a person deciding
  anything: a bug with a reproduction, a small addition with a clear
  definition of done, yes; a design or product decision, visual judgement,
  a system the agent cannot reach, no;
- a confidence, a reason a developer can check, and a first-look plan.

Tickets it could take say **I can fix this**. Tick any — those or others —
and **Fix N selected** starts one agent per ticket, up to the number set
under **At once**, the rest queued. Each agent:

1. checks out a fresh branch (`fix/abc-7-crash-on-empty-repo`) from the
   default branch in its **own worktree**, so agents cannot see each other's
   half-written files;
2. runs the coding agent on the ticket, live, with the repository's own
   checks from `.git-manage-ci.toml`;
3. runs those checks again itself once the agent says it is done — a draft
   pull request is never opened on the agent's word that the tests passed.
   A repository that declares no checks gets the ones its toolchain
   implies: `flutter analyze` and `flutter test` for a Flutter app, `cargo
   build` and `cargo test`, `npm test`, `pytest`, `go test`. Projects are
   found at the root or up to three directories down (a Flutter app under
   `mobile/`), and each check runs in its project's directory; the log
   says they were inferred;
4. if a check fails, sends the failure back to the agent for another
   **round**, up to the number set in the dialog (three by default, up to
   ten). The failure does not go back alone: a second agent — the
   reviewer's engine, or the fixer's own in a fresh session — reads the
   attempt, the output, and the code, and says what went wrong and what to
   do; the fixer gets both, with the thread of every round so far. The
   same happens when the agent changed nothing and said it needs a person:
   the second agent looks, and either sends it back with instructions or
   agrees, which ends the run with both opinions on the card. A round that
   leaves the tree exactly as the last one did ends the run too. No pull
   request is opened while a check fails;
5. once the checks pass, has a **second agent review** the ticket and the
   diff — the code-review model, which can be Claude Code — and answer
   approve or revise. Revise is another round with the feedback; approve
   is recorded in the pull request. Untick **Second agent reviews** to
   skip this;
6. commits, pushes, and opens a **draft pull request** that quotes the
   ticket, the agent's summary, which checks passed, and who approved it
   after how many rounds;
7. **removes the worktree**. The branch and the pull request are what
   remain.

A ticket the agent changed nothing for, or whose change fails a check,
leaves nothing behind: the worktree is removed and the branch deleted. The
card in the dialog says why, and **Copy log** puts the whole log on the
clipboard.

Both agents are held to one standard whatever the language: reusable over
ad hoc (logic needed twice lives once; a helper the repository has is used,
not rewritten), one responsibility per function and type with behaviour
kept with its data, clear names and no magic numbers or dead code, the
repository's own conventions, and a test for what changed. The fixer is
told to write to it; the reviewer sends back what does not meet it.

The dialog is the place to watch: every agent has a card with its state
(queued, running, done, failed), how long it has run, what it is doing right
now, and — on **Log** — everything it did: every file read, every edit, every
check, the commit, the push. A finished card lists the files it changed with
line counts, its summary, and the pull request.

### Claiming the ticket

**Claim tickets I start** (on by default) does what you would do when you
pick a ticket up: the moment an agent starts on it, the ticket is assigned
to you, moved into the project's active sprint, and transitioned to In
Progress — so it leaves the backlog while it is being worked, and the board
shows who has it. When the agent finishes, a comment on the ticket links the
draft pull request; when it gives up, the comment says why, and the ticket
stays assigned to you. Each step is a line on the agent's card, and none of
them can fail the fix.

A kanban project has no sprints, and a workflow may have no In Progress
step; both are skipped and said so. Untick the box and **nothing is written
to Jira**: the ticket stays unassigned and open, and what happens to it is
the developer's call after reviewing the pull request. On the command line
the same is `--claim`, off unless given.

### The model

The backlog fixer has its own model setting — **Settings → Models per task
→ The backlog fixer**, or the picker in the dialog. It is the coding agent
running unattended, so point it at the strongest model you have. It falls
back to the coding agent's model until one is chosen.

**Claude Code** is an option too, when the `claude` command is installed:
the fix then runs Anthropic's own agent headless in the worktree, with a
shell but no git history commands
(see [the engine](editor-and-agent.md#the-engine-the-built-in-harness-or-claude-code)),
and everything else — the
verification, the commit, the draft pull request, the worktree removal — is
unchanged. Judging the backlog is a read-only harness run, so it uses the
nearest task's model (Jira tickets, the coding agent, …) instead.

### The sandbox

**Run checks in a sandbox** runs every check the agent triggers, and the
verification afterwards, inside a Docker image with the worktree mounted at
`/work`. A build or a test suite then cannot touch the machine. The image
is guessed from the repository (`rust:1-bookworm` for a Cargo project,
`node:22-bookworm`, `python:3.12-bookworm`, …) and can be changed; the
setting is on by default when Docker is available and the repository has a
recognisable toolchain. Checks that already name an `image` in
`.git-manage-ci.toml` keep theirs.

It does not accumulate: each container runs with `--rm`, so it and its
anonymous volumes are deleted when it exits; build output goes into the
mounted worktree, which is deleted with it; and every check has a timeout
(`timeout_secs` per job, 30 minutes otherwise) after which the container is
removed by name and the check fails. What stays is the image itself, which
is the cache — `docker image prune` reclaims it. A fresh container has no
dependency cache, so a Rust or Node build downloads its dependencies every
run; that is the price of a clean room.

## What it does not do

- **Browse Jira.** The backlog view reads one project's unassigned tickets
  and nothing else; a client that also tried to be a Jira browser would be
  a worse version of the one you have.
- **Resolve tickets.** Claiming assigns, sprints, starts, and comments;
  it never closes. The pull request merging is a decision, and so is
  closing the ticket.
- **Epics, sprints, or estimates.** A ticket is created with a project, a
  type, a summary, a description and labels; the rest is a workflow question
  each team answers differently.
- **Jira Server or Data Center.** The API here is Cloud's v3.
