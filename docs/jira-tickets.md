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

## What it does not do

- **Read or search Jira.** This exists to put work in. A client that also
  tried to be a Jira browser would be a worse version of the one you have.
- **Epics, sprints, or estimates.** A ticket is created with a project, a
  type, a summary, a description and labels; the rest is a workflow question
  each team answers differently.
- **Jira Server or Data Center.** The API here is Cloud's v3.
