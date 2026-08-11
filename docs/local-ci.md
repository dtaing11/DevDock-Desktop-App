# Local CI Guide

Git Manage can run your checks **locally before you create a pull request**,
similar in spirit to GitHub Actions but on your own machine, with optional
Docker containers for reproducible environments.

- [Quick start](#quick-start)
- [The config file](#the-config-file)
- [Running checks](#running-checks)
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
- A failing check does **not** block PR creation; you get a clear red
  warning and can decide.

## Docker environments

Add `image = "..."` to run a job inside a container:

```toml
[[job]]
name = "tests on Debian stable"
image = "rust:1.80-slim-bookworm"
commands = ["cargo test"]
```

What happens under the hood:

```sh
docker run --rm \
  -v /path/to/your/repo:/work \
  -w /work \
  -e KEY=VALUE ... \
  rust:1.80-slim-bookworm sh -c "cargo test"
```

- The repository is mounted read-write at `/work`.
- The container is removed afterwards (`--rm`), each run starts clean.
- Any public or private image you can `docker pull` works: specific
  distro versions, toolchain versions, your own base images.

Useful images:

| Purpose            | Image                        |
|--------------------|------------------------------|
| Rust               | `rust:1.80`, `rust:latest`   |
| Node.js            | `node:22`, `node:20-alpine`  |
| Python             | `python:3.12`, `python:3.12-slim` |
| Go                 | `golang:1.23`                |
| Ubuntu base        | `ubuntu:24.04`               |
| Debian base        | `debian:bookworm`            |
| Alpine (small)     | `alpine:3.20`                |

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

Both can coexist: run checks locally before the PR, and let the hosted
workflow (`.github/workflows/ci.yml`) validate after pushing.

## Limitations

- **Docker containers are Linux.** True macOS or Windows runners cannot be
  emulated locally by Docker; use hosted CI for those targets. Any Linux
  distro/version/toolchain image works fine.
- Jobs run in parallel with no dependency graph (no `needs:` equivalent).
  If job B needs job A, put both command lists in one job.
- Caching is whatever the host/container sees. Host jobs reuse your local
  caches (fast); fresh containers download dependencies each run unless you
  bake them into a custom image.

## Building on top of the engine

The CI engine is a library with a pluggable `Runner` trait: add SSH/Podman/
custom environments, override built-ins, or embed the engine in your own
tools. See [extending-local-ci.md](extending-local-ci.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Docker is not available` on a container job | Install Docker (or Docker Desktop / colima on macOS) and make sure `docker --version` works in a terminal |
| `Missing secret(s): X` | Add `X = value` to `.git-manage-ci.secrets` or `export X=...` before launching the app |
| Container job can't find your toolchain | The container only has what the image ships; pick a toolchain image (`rust:1.80`, `node:22`) or install inside `commands` |
| Changes not picked up after editing the config | Click **Reload** in the LOCAL CHECKS section |
| Job output cut off | Output is capped at 64 KB per job to keep the UI responsive; run the command in a terminal for the full log |
