# Local CI Guide

Git Manage can run your checks **locally before you create a pull request**,
similar in spirit to GitHub Actions but on your own machine, with optional
Docker containers for reproducible environments.

- [Quick start](#quick-start)
- [The config file](#the-config-file)
- [Running checks](#running-checks)
- [Gating pushes and pull requests](#gating-pushes-and-pull-requests)
- [AI code review](#ai-code-review)
  - [Before you start: sign in to a model](#before-you-start-sign-in-to-a-model)
  - [Enabling it](#enabling-it)
  - [Severity and the `fail_on` threshold](#severity-and-the-fail_on-threshold)
  - [Reading a review, and overriding it](#reading-a-review-and-overriding-it)
  - [Choosing a provider and model](#choosing-a-provider-and-model)
  - [Project-specific instructions](#project-specific-instructions)
  - [Custom output format](#custom-output-format)
  - [What the reviewer sees](#what-the-reviewer-sees)
- [Docker environments](#docker-environments)
- [Secrets](#secrets)
- [Recipes](#recipes)
  - [Unit + integration tests](#unit--integration-tests)
  - [Lint, format, and test matrix](#lint-format-and-test-matrix)
  - [Integration tests against a database](#integration-tests-against-a-database)
  - [API integration tests with a secret token](#api-integration-tests-with-a-secret-token)
  - [Node, Python, Go examples](#node-python-go-examples)
- [How it maps to GitHub Actions](#how-it-maps-to-github-actions)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)

---

## Quick start

1. Open your repository in Git Manage.
2. Click **Pull Request** in the toolbar.
3. In the **LOCAL CHECKS** section, click **Create config**.
   This writes `.git-manage-ci.toml` to your repository root.
4. Edit the file (any editor) to define your jobs.
5. Back in the PR dialog, click **Reload**, then **Run all checks**.

Each job shows `[pending]` → `[running]` → `[pass 2.3s]` or `[fail 0.8s]`.
Click a job's name to expand its full output log.

From there you can go two ways, independently:

- Make the checks run automatically and gate a push — see
  [Gating pushes and pull requests](#gating-pushes-and-pull-requests).
- Add an AI review of the diff before it is published — see
  [AI code review](#ai-code-review). That one needs a model set up first:
  [Before you start](#before-you-start-sign-in-to-a-model).

The starter config written by **Create config** includes both as commented-out
examples, so you can uncomment rather than type them from scratch.

## The config file

`.git-manage-ci.toml` lives at the repository root and is meant to be
**committed**, so your whole team shares the same checks.

```toml
# Every [[job]] block is one check. Jobs run in parallel.

[[job]]
name = "tests"                      # shown in the UI
commands = [                        # run in order, stop at first failure
  "cargo build",
  "cargo test",
]

[[job]]
name = "lint"
commands = ["cargo clippy -- -D warnings"]
image = "rust:1.80"                 # optional: run inside this Docker image
env = { RUST_BACKTRACE = "1" }      # optional: extra environment variables
secrets = ["API_TOKEN"]             # optional: secret names (values elsewhere)
```

| Field      | Required | Meaning                                                            |
|------------|----------|--------------------------------------------------------------------|
| `name`     | yes      | Label shown in the PR dialog                                       |
| `commands` | yes      | Shell commands joined with `&&` (first failure stops the job)      |
| `image`    | no       | Docker image; when set, the job runs in a container                |
| `env`      | no       | Plain environment variables (committed, never put secrets here)    |
| `secrets`  | no       | Names of secrets to inject; values come from the secrets file      |

## Running checks

- **Run all checks** runs every job **in parallel** on background threads.
  Keep jobs independent of each other.
- The whole repository working tree is the job's working directory,
  including uncommitted changes, so you test exactly what you are about
  to push.
- Run manually, a failing check does **not** block anything; you get a clear
  red warning and decide what to do. Configure `[on_push]` below to make
  failures actually gate a push.

## Gating pushes and pull requests

By default the jobs only run when you ask them to. Add an `[on_push]` section
to run them automatically before every push from the app:

```toml
[on_push]
run = true                # run all jobs before each push
block_on_failure = true   # a failing job cancels the push
```

| Field              | Default | Meaning                                                     |
|--------------------|---------|-------------------------------------------------------------|
| `run`              | `false` | Run every job automatically before a push                   |
| `block_on_failure` | `true`  | A failing job cancels the push instead of only warning      |

With `run = true`, pushing switches to the **Checks** tab and runs the jobs
first. If they pass, the push proceeds. If one fails and
`block_on_failure = true`, the push is cancelled and the first failing job is
expanded for you. Set `block_on_failure = false` to be warned but pushed
anyway.

## AI code review

Separately from the jobs, Git Manage can have an AI model review the diff you
are about to publish and report what it finds. It runs **after** the jobs
pass, on both pushes and pull requests.

It is advisory by design. It reports findings with its reasoning, and you can
always proceed anyway — see [Reading a review, and overriding
it](#reading-a-review-and-overriding-it).

### Before you start: sign in to a model

**The reviewer needs a working model before any of the config below does
anything.** DevDock does not ship one. Set up whichever provider you intend to
use first, then come back and enable `[review]`.

Without this, the review is skipped with *"AI review is enabled but no model is
available"* and your push or pull request proceeds unreviewed. That is
deliberate — the gate never blocks work because its own dependency is missing —
but it does mean a misconfigured reviewer is easy to miss. If you never see
review output, check here first.

#### Option A — Claude (Anthropic)

Open **Settings (⚙)** → the Claude section, and use either:

- **Browser sign-in.** Click sign-in, approve access in the browser tab that
  opens, and paste the code shown after approval. This uses your Claude
  subscription.
- **API key.** Paste a key (`sk-ant-…`) from
  [console.anthropic.com](https://console.anthropic.com) into the API-key
  field. This bills per token against your Anthropic account.

Then pick a model, or pin one in the config with `provider = "claude"` and
`model = "…"`.

> ⚠️ **Subscription sign-in and large models.** A Claude subscription meters
> Opus and Sonnet on much smaller quotas than Haiku, and reviewing a whole diff
> is a token-heavy request. If you pin a large model and keep hitting the cap,
> the client falls back to Haiku so the review still happens — so a review may
> come from a smaller model than the one you configured. An API key is the way
> to use a large model consistently.

#### Option B — Ollama (local)

1. **Install [Ollama](https://ollama.com)** and confirm it runs:
   `ollama --version`.
2. **Pull a model your machine can actually run**, e.g.
   `ollama pull llama3.2`. Check it is there with `ollama list`.
3. In **Settings (⚙)**, confirm the server URL (default
   `http://localhost:11434`) and pick the model.

Two things specific to reviewing locally:

- **Size the model to your hardware.** A model larger than your available RAM
  or VRAM will either fail to load or run too slowly to be useful, and reviews
  send a whole diff rather than a few lines. If a review times out or the
  machine grinds, use a smaller model or lower `max_diff_bytes`.
- **Small models often cannot hold the findings format.** The default output is
  structured JSON, and a small local model frequently produces something
  unparseable — you will see *"did not return a usable review"*. Either use a
  larger model, switch to `provider = "claude"`, or use
  [`output = "markdown"`](#custom-output-format), which has no format to parse
  and is much more forgiving.

#### If you leave `provider` and `model` out

The reviewer uses whatever you selected for AI features in the app, so it
follows your existing sign-in. Pin both in the config when everyone on the team
should review with the same model — see [Choosing a provider and
model](#choosing-a-provider-and-model).

### Enabling it

Add a `[review]` section to the same `.git-manage-ci.toml`:

```toml
[review]
run = true                # review before every push AND pull request
block_on_failure = true   # findings at or above fail_on stop to ask first
fail_on = "high"          # low | medium | high

# Optional:
# on_push = true                       # gate each trigger on its own
# on_pull_request = true
# provider = "claude"                  # claude | ollama
# model = "claude-opus-5"
# max_diff_bytes = 24000               # cap on how much diff is sent
# instructions = "Flag any new blocking call on the UI thread."
```

`run = true` is the shorthand for "both triggers". To review one and not the
other, set the trigger directly:

```toml
# Review pull requests only. Everyday pushes go straight through.
[review]
on_pull_request = true
```

```toml
# Review every push, but not PR creation (the code was already reviewed).
[review]
on_push = true
```

```toml
# Both, except pushes — an explicit false overrides `run`.
[review]
run = true
on_push = false
```

Each trigger falls back to `run` when left out, so a config that only sets
`run` behaves exactly as before. `on_pr` is accepted as a spelling of
`on_pull_request`. The **AI review** button in the Checks tab ignores all of
this — asking for a review is its own consent.

| Field              | Default  | Meaning                                                            |
|--------------------|----------|--------------------------------------------------------------------|
| `run`              | `false`  | Review before pushes **and** pull requests                         |
| `on_push`          | `run`    | Review before a push, overriding `run`                             |
| `on_pull_request`  | `run`    | Review before creating a PR, overriding `run` (alias: `on_pr`)     |
| `block_on_failure` | `true`   | Findings at or above `fail_on` stop and ask before proceeding      |
| `fail_on`          | `"high"` | Lowest severity that stops to ask (`low`, `medium`, `high`)        |
| `provider`         | app's    | `claude` or `ollama`; defaults to your selection in the app        |
| `model`            | app's    | Model for that provider                                            |
| `max_diff_bytes`   | `24000`  | Diff is truncated past this, with a marker so the model knows      |
| `instructions`     | none     | Extra project-specific things to look for                          |
| `output`           | `"findings"` | `"markdown"` to answer in your own format — see [Custom output format](#custom-output-format) |
| `output_instructions` | none  | The house style for `output = "markdown"`                          |

Omitting `[review]` entirely leaves the reviewer off, so adding it to an
existing repository changes nothing until you opt in.

You can also run a review at any time without gating anything: the **AI
review** button in the Checks tab reports on the current outgoing diff, and
works even with `run = false`.

### Severity and the `fail_on` threshold

Every finding carries one of three severities:

| Severity | Means                                                                                       |
|----------|---------------------------------------------------------------------------------------------|
| `high`   | The change is incorrect or unsafe: wrong results, crashes, data loss, races, leaks, injection, auth or secret exposure, a broken API contract |
| `medium` | A real problem that is not a correctness failure: missing error handling on a path that can fail, a missing test for new branching logic, a performance cliff, a misleading name |
| `low`    | Style, naming, formatting, preference                                                       |

`fail_on` sets the lowest severity that stops to ask you. With the default
`"high"`, medium and low findings are reported but never interrupt a push.

The model is deliberately asked to report **every** finding at its honest
severity, and this threshold does the filtering. That is why `fail_on` exists
rather than an instruction to "only report serious issues": a model told to
self-censor investigates just as hard and then withholds the rest, which
reads back as a clean review of code that is not clean.

Tighten to `fail_on = "medium"` on code you want held to a higher bar. Set
`block_on_failure = false` to get reviews without any interruption.

### Reading a review, and overriding it

When findings reach the threshold, a dialog holds the push or pull request
and shows:

- the tally (`3 high · 1 medium · 0 low`) and how many crossed `fail_on`
- a one-line **summary** of the change's state
- **Reviewer's reasoning** — what it examined, what it is confident is
  correct, and what it could not verify from the diff alone
- each finding: severity, `file:line`, a one-line title, and an expandable
  detail with the failing case and a suggested fix

Two choices:

- **Cancel and fix** — abandons the push/PR. Findings stay in the Checks tab.
- **Push anyway** / **Create pull request anyway** — proceeds regardless.

The override is a plain button, not a hidden setting, because the reviewer
can be wrong. Reading the reasoning is what makes overriding a judgement
rather than a coin flip. Closing the dialog with the X is the same as
Cancel — dismissing a blocking review is never treated as approval.

Findings persist in the Checks tab after the dialog closes, so a review you
overrode is still there to come back to.

A review that **cannot run** — no provider signed in, request failed, model
returned something unusable — reports the reason and lets the action through.
It never blocks on its own failure.

### Choosing a provider and model

With no `provider`/`model` set, the reviewer uses whatever you selected for
AI features in the app, so it follows your Claude or Ollama sign-in. Pin them
in the config when the whole team should review with the same model:

```toml
[review]
run = true
provider = "claude"
model = "claude-opus-5"
```

Reviewing is the most demanding AI task in the app: it has to hold a whole
diff in context and return structured JSON. Small local models often cannot
keep to the format — if you see "did not return a usable review", try a
larger Ollama model or switch to `provider = "claude"`.

### Project-specific instructions

`instructions` is appended to the prompt. Use it for rules a reviewer could
not infer from the diff alone:

```toml
[review]
run = true
instructions = """
Flag any new blocking I/O on the UI thread — this app has a single render
loop and a blocked frame is a visible freeze.
Flag new `unwrap()` outside tests.
Database migrations must be reversible; flag any that are not.
"""
```

Keep these to genuine project rules. Restating general good practice adds
tokens without changing the review.

### Custom output format

By default the reviewer answers in a fixed structure and the app renders the
findings list itself. Set `output = "markdown"` to have it answer in **your**
format instead, rendered as Markdown in the app:

```toml
[review]
run = true
output = "markdown"
output_instructions = """
## Verdict
One sentence: ship it, or don't.

## Must fix
Bullets. Each one gives `file:line`, what breaks, and the input that breaks it.

## Nits
Bullets, or the single word "none".

Keep the whole thing under 200 words. No preamble, no praise.
"""
```

| Field                 | Meaning                                                                 |
|-----------------------|-------------------------------------------------------------------------|
| `output`              | `"findings"` (default) or `"markdown"`                                  |
| `output_instructions` | The house style: sections, tone, length. Ignored in findings mode.      |

What is rendered: headings, `**bold**`, `*italic*`, `` `inline code` ``,
fenced code blocks (syntax-highlighted with the same highlighter as the diff
views), bulleted and numbered lists, blockquotes, horizontal rules, and links.
Anything outside that subset falls back to plain text — nothing is dropped.

The review criteria do not change between modes. Only the output contract
does, so you still get correctness-first review, concrete failing cases, and
no commentary on untouched code.

**Gating in this mode works differently.** There are no severities to compare
against `fail_on`, so with `block_on_failure = true` the model is required to
lead with one of:

```
VERDICT: block
VERDICT: pass
```

That line drives the gate and is stripped before your review is shown — it
never appears in the rendered output. `fail_on` is ignored in this mode.

With `block_on_failure = false` no verdict line is requested at all, and the
review is purely advisory: it appears in the Checks tab and nothing is ever
held. That is the simplest setup if you want a second opinion in your own
format without any interruption.

> A local model that cannot reliably produce the structured findings JSON may
> still do well here, since there is no format to parse. If you saw
> "did not return a usable review" with Ollama, this mode is worth trying.

### What the reviewer sees

The reviewer reads the **committed work you are about to publish**, not your
working tree:

| Action                          | Diff reviewed                                          |
|---------------------------------|--------------------------------------------------------|
| Pull request                    | `base...HEAD` — against the PR's target branch         |
| Push, branch has an upstream    | `@{upstream}...HEAD` — the commits that would be sent  |
| Push, no upstream yet           | Everything on the branch that no remote has            |

Uncommitted edits are **not** reviewed, because a push does not publish them.
This is the opposite of the jobs, which run against the working tree. If you
want a review of work in progress, commit it first (or use the Checks tab
button after committing).

## Docker environments

Add `image = "..."` to run a job inside a container:

```toml
[[job]]
name = "tests on Debian stable"
image = "rust:1.88-slim-bookworm"
commands = ["cargo test"]
```

What happens under the hood:

```sh
docker run --rm \
  -v /path/to/your/repo:/work \
  -w /work \
  -e KEY=VALUE ... \
  rust:1.88-slim-bookworm sh -c "cargo test"
```

- The repository is mounted read-write at `/work`.
- The container is removed afterwards (`--rm`), each run starts clean.
- Any public or private image you can `docker pull` works: specific
  distro versions, toolchain versions, your own base images.

Useful images:

| Purpose            | Image                        |
|--------------------|------------------------------|
| Rust               | `rust:latest`, `rust:1.88`   |
| Node.js            | `node:22`, `node:20-alpine`  |
| Python             | `python:3.12`, `python:3.12-slim` |
| Go                 | `golang:1.23`                |
| Ubuntu base        | `ubuntu:24.04`               |
| Debian base        | `debian:bookworm`            |
| Alpine (small)     | `alpine:3.20`                |

Two things that bite when pinning a toolchain version:

- **Pin at or above your dependencies' minimum.** A container job with an older
  toolchain than any dependency requires fails before compiling a line of your
  code (`package X requires rustc 1.88 or newer`). Your host toolchain being
  newer hides this, so the job fails while `cargo test` passes locally. Find the
  floor with:

  ```sh
  cargo metadata --format-version 1 \
    | python3 -c 'import json,sys; print(max((p["rust_version"] for p in json.load(sys.stdin)["packages"] if p.get("rust_version")), key=lambda v: [int(x) for x in v.split(".")]))'
  ```

- **`-slim` and `-alpine` images have no C toolchain.** Anything with a C
  dependency (crypto, fonts, native bindings) fails on `cc`, `ld`, or
  `pkg-config`. Use the non-slim tag, or install what you need as the job's
  first command:

  ```toml
  commands = [
    "apt-get update -qq && apt-get install -y -qq --no-install-recommends build-essential pkg-config",
    "cargo test",
  ]
  ```

## Secrets

Some tests need credentials (API tokens, database passwords). Never put
them in `.git-manage-ci.toml`, since that file is committed. Instead:

1. Declare the **names** in the job:

   ```toml
   [[job]]
   name = "integration tests"
   commands = ["./scripts/integration-tests.sh"]
   secrets = ["API_TOKEN", "DB_PASSWORD"]
   ```

2. Put the **values** in `.git-manage-ci.secrets` at the repo root
   (KEY=VALUE lines, `#` for comments):

   ```ini
   # local secrets, never committed
   API_TOKEN = ghp_example123
   DB_PASSWORD = hunter2
   ```

3. Add the secrets file to `.gitignore`:

   ```gitignore
   .git-manage-ci.secrets
   ```

Resolution order per secret name:

1. `.git-manage-ci.secrets` file
2. Your shell environment (`export API_TOKEN=...` before launching the app)

If a declared secret can't be found in either place, the job **fails
immediately with a message naming the missing secret**, so you never run a
half-configured integration suite. Secrets are injected as environment
variables both on the host and inside Docker containers.

## Recipes

### Unit + integration tests

```toml
[[job]]
name = "unit tests"
commands = ["cargo test --lib"]

[[job]]
name = "integration tests"
commands = ["cargo test --test '*' -- --test-threads=1"]

[[job]]
name = "doc tests"
commands = ["cargo test --doc"]
```

Three parallel jobs with separate pass/fail status and logs.

### Lint, format, and test matrix

```toml
[[job]]
name = "format"
commands = ["cargo fmt --check"]

[[job]]
name = "clippy"
commands = ["cargo clippy --all-targets -- -D warnings"]

[[job]]
name = "tests (stable)"
image = "rust:latest"
commands = ["cargo test"]

[[job]]
name = "tests (1.75 MSRV)"
image = "rust:1.75"
commands = ["cargo test"]
```

The two container jobs emulate a version matrix, like an Actions
`strategy.matrix` across toolchains.

### Integration tests against a database

Start the database as part of the job (host jobs can use your local
Docker directly):

```toml
[[job]]
name = "postgres integration"
commands = [
  "docker run -d --rm --name ci-pg -e POSTGRES_PASSWORD=$DB_PASSWORD -p 15432:5432 postgres:16",
  "sleep 3",
  "DATABASE_URL=postgres://postgres:$DB_PASSWORD@localhost:15432/postgres cargo test --test db_integration; status=$?; docker stop ci-pg; exit $status",
]
secrets = ["DB_PASSWORD"]
```

Note the pattern in the last command: capture the test exit code, stop the
container, then exit with the test status so cleanup always happens.

### API integration tests with a secret token

```toml
[[job]]
name = "API smoke tests"
image = "python:3.12-slim"
commands = [
  "pip install -q requests pytest",
  "pytest tests/api_smoke.py -q",
]
secrets = ["API_TOKEN"]
env = { API_BASE_URL = "https://staging.example.com" }
```

`API_TOKEN` comes from your secrets file; `API_BASE_URL` is not secret so
it can live in the committed config.

### Node, Python, Go examples

```toml
[[job]]
name = "node tests"
image = "node:22"
commands = ["npm ci", "npm test"]

[[job]]
name = "python tests"
image = "python:3.12"
commands = ["pip install -e .[test]", "pytest -q"]

[[job]]
name = "go tests"
image = "golang:1.23"
commands = ["go vet ./...", "go test ./..."]
```

## How it maps to GitHub Actions

| GitHub Actions            | Git Manage local CI                          |
|---------------------------|----------------------------------------------|
| `jobs.<id>`               | `[[job]]` block                              |
| `jobs.<id>.name`          | `name`                                       |
| `steps.run`               | entries in `commands`                        |
| `runs-on` / `container`   | `image` (Docker), or omit for host           |
| `env`                     | `env`                                        |
| `secrets.*`               | `secrets` + `.git-manage-ci.secrets` file    |
| matrix                    | multiple jobs with different `image`s        |
| branch protection rules   | `[on_push] block_on_failure`                 |
| a review-bot Action       | `[review]` (runs before the push, not after) |

Both can coexist: run checks locally before the PR, and let the hosted
workflow (`.github/workflows/ci.yml`) validate after pushing.

The `[review]` gate differs from a review bot in one way worth noting: it runs
*before* the code leaves your machine, so findings never appear in the PR's
public history. Nothing is posted to GitHub.

## Limitations

- **Docker containers are Linux.** True macOS or Windows runners cannot be
  emulated locally by Docker; use hosted CI for those targets. Any Linux
  distro/version/toolchain image works fine.
- Jobs run in parallel with no dependency graph (no `needs:` equivalent).
  If job B needs job A, put both command lists in one job.
- Caching is whatever the host/container sees. Host jobs reuse your local
  caches (fast); fresh containers download dependencies each run unless you
  bake them into a custom image.
- The AI reviewer sees the diff and nothing else — not the surrounding files,
  not the tests, not the build. It cannot tell you whether the change
  actually works; that is what the jobs are for. Treat it as a second pair of
  eyes on the patch, not a substitute for running the tests.
- Large diffs are truncated at `max_diff_bytes`. A very large change is
  reviewed only in part, and the model is told so.

## Building on top of the engine

The CI engine is a library with a pluggable `Runner` trait: add SSH/Podman/
custom environments, override built-ins, or embed the engine in your own
tools. See [extending-local-ci.md](extending-local-ci.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Docker is installed but its daemon is not running` | Start Docker Desktop (macOS/Windows) or `sudo systemctl start docker` (Linux). `docker --version` succeeding is **not** enough — it only prints the client version; `docker info` is the check that needs the daemon |
| `Docker is not installed, or docker is not on PATH` | Install Docker Desktop (or colima / podman-docker), or drop `image` from the job so it runs on this machine |
| `Cannot connect to the Docker daemon` in a job's stderr | The daemon stopped after the pre-flight check, or something else is holding the socket. Start Docker and re-run |
| Container job fails before compiling (`requires rustc 1.xx or newer`) | The image's toolchain is older than a dependency's minimum. Check with `cargo metadata` and pin a newer tag, e.g. `rust:1.88-slim-bookworm` |
| Container job fails on `cc`, `ld`, or `pkg-config` | `-slim` images ship no C toolchain. Use the non-slim tag, or `apt-get install -y build-essential pkg-config` as the job's first command |
| `Missing secret(s): X` | Add `X = value` to `.git-manage-ci.secrets` or `export X=...` before launching the app |
| Container job can't find your toolchain | The container only has what the image ships; pick a toolchain image (`rust:1.80`, `node:22`) or install inside `commands` |
| Changes not picked up after editing the config | Click **Reload** in the LOCAL CHECKS section |
| Job output cut off | Output is capped at 64 KB per job to keep the UI responsive; run the command in a terminal for the full log |
| `AI review is enabled but no model is available` | No provider is set up. Sign in to Claude in Settings, or install Ollama and pull a model — see [Before you start](#before-you-start-sign-in-to-a-model). The push still goes through |
| `did not return a usable review` | The model could not hold the JSON format — usually a small local model. Use a larger one, set `provider = "claude"`, or switch to `output = "markdown"`, which has no format to parse |
| `Claude is not signed in. Open Settings.` | Browser sign-in or an API key is missing. `[review] provider = "claude"` needs one of the two |
| Ollama review times out, or the machine grinds | The model is too large for your hardware, or the diff is. Pull a smaller model, or lower `max_diff_bytes` |
| `Ollama request failed` | Ollama is not running or is on another port. Check `ollama list`, then the server URL in Settings (default `http://localhost:11434`) |
| Review came from a different model than configured | On a Claude subscription, Opus/Sonnet caps are much lower than Haiku's; the client falls back to Haiku when the cap is hit. Use an API key for a large model consistently |
| `Nothing to review: no outgoing changes found` | Everything on the branch is already pushed, or your work is still uncommitted — the reviewer reads commits, not the working tree |
| Review never runs on push | Check `[review] run = true` (or `on_push = true`), and that `on_push` is not explicitly `false`. It runs *after* the jobs, so a failing job with `[on_push] block_on_failure = true` cancels the push before the review starts |
| Review runs on push but not on PR (or vice versa) | One trigger is off. `run` covers both; `on_push` / `on_pull_request` override it per trigger |
| Review interrupts too often | Raise `fail_on` (e.g. to `"high"`), or set `block_on_failure = false` to get reviews without interruption |
