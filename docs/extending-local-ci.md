# Extending DevDock Local CI

The local CI engine is built as a library with pluggable **runners**, so
developers can build on top of it: add new execution environments, embed
the engine in other tools, or drive it programmatically.

- [Architecture](#architecture)
- [The Runner trait](#the-runner-trait)
- [Writing a custom runner](#writing-a-custom-runner)
  - [Example: SSH runner](#example-ssh-runner)
  - [Example: Podman runner](#example-podman-runner)
- [Using custom runners from the config](#using-custom-runners-from-the-config)
- [Embedding the engine](#embedding-the-engine)
- [Extending the config](#extending-the-config)
- [Testing your runner](#testing-your-runner)
- [Design contract](#design-contract)

## Architecture

```
.git-manage-ci.toml ──► Config { jobs, on_push }
                              │
                              ▼
        run_job_with(&RunnerRegistry, repo_root, &Job)
                              │
              ┌───────────────┼─────────────────┐
              ▼               ▼                 ▼
         HostRunner      DockerRunner      YourRunner
         (sh -c)         (docker run)      (anything)
```

Everything lives in `src/local_ci/`:

| Piece | File | Role |
|-------|------|------|
| `Config`, `Job`, `OnPush` | `mod.rs` | Parsed `.git-manage-ci.toml` |
| `run_job` / `run_job_with` | `mod.rs` | Orchestration: resolve runner, env, secrets; collect output |
| `Runner` trait | `runner.rs` | Where commands execute |
| `HostRunner`, `DockerRunner` | `runner.rs` | Built-in environments |
| `RunnerRegistry` | `runner.rs` | Id → runner lookup, extensible |

The orchestration layer owns everything that should behave identically
regardless of environment: secrets resolution, env merging, first-failure
semantics (`&&`), output capping, and timing. A runner only answers one
question: *run this script over there and tell me what happened*.

## The Runner trait

```rust
pub trait Runner: Send + Sync {
    /// Id referenced by jobs: `runner = "<id>"`.
    fn id(&self) -> &'static str;

    /// Prerequisite check; an Err fails the job with this message.
    fn available(&self) -> Result<(), String>;

    /// Execute the script, report success + stdout/stderr.
    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String>;
}
```

`ExecRequest` gives you:

| Field | Meaning |
|-------|---------|
| `repo_root` | Absolute path of the repository worktree |
| `script` | The job's commands joined with `&&` |
| `env` | Config `env` plus resolved secrets, ready to inject |
| `target` | The job's `runner_target` (or `image` as fallback) |

`Send + Sync` is required because jobs run concurrently on worker threads.

## Writing a custom runner

### Example: SSH runner

Run jobs on a remote build machine:

```rust
use git_manage::local_ci::{ExecOutput, ExecRequest, Runner};
use std::process::Command;

struct SshRunner;

impl Runner for SshRunner {
    fn id(&self) -> &'static str {
        "ssh"
    }

    fn available(&self) -> Result<(), String> {
        Command::new("ssh")
            .arg("-V")
            .output()
            .map(|_| ())
            .map_err(|_| "ssh is not installed".to_string())
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        let host = request.target.ok_or("ssh runner needs runner_target = \"user@host\"")?;

        // Forward env as `KEY=VALUE cmd` prefixes (quote for your threat model!)
        let exports: String = request
            .env
            .iter()
            .map(|(k, v)| format!("{k}='{v}' "))
            .collect();

        let output = Command::new("ssh")
            .arg(host)
            .arg(format!("{exports}{}", request.script))
            .output()
            .map_err(|e| e.to_string())?;

        Ok(ExecOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
```

### Example: Podman runner

Docker-compatible, rootless. Because `register` **replaces runners with the
same id**, you can even override the built-in `docker` id:

```rust
struct PodmanRunner;

impl Runner for PodmanRunner {
    fn id(&self) -> &'static str {
        "docker" // override the built-in: jobs with `image` now use podman
    }

    fn available(&self) -> Result<(), String> {
        std::process::Command::new("podman")
            .arg("--version")
            .output()
            .map(|_| ())
            .map_err(|_| "podman not installed".to_string())
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        let image = request.target.ok_or("needs an image")?;
        let output = std::process::Command::new("podman")
            .args(["run", "--rm", "-v"])
            .arg(format!("{}:/work", request.repo_root.display()))
            .args(["-w", "/work", image, "sh", "-c", request.script])
            .output()
            .map_err(|e| e.to_string())?;
        Ok(ExecOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
```

## Using custom runners from the config

Jobs pick runners by id; `runner_target` carries environment-specific
configuration:

```toml
[[job]]
name = "tests on build box"
runner = "ssh"
runner_target = "builder@10.0.0.5"
commands = ["cd /srv/app && cargo test"]
secrets = ["DEPLOY_KEY"]

[[job]]
name = "container tests"
image = "rust:1.80"          # `image` implies runner = "docker"
commands = ["cargo test"]

[[job]]
name = "plain host job"       # no runner, no image -> "host"
commands = ["cargo fmt --check"]
```

Resolution rules (in `Job::runner_id`):

1. Explicit `runner = "..."` wins.
2. Otherwise `image = "..."` implies `docker`.
3. Otherwise `host`.

Unknown runner ids fail the job with a message listing available ids.

## Embedding the engine

The `git_manage` crate is a library; the CI engine has no UI dependencies.
Minimal embedding:

```rust
use git_manage::local_ci::{self, RunnerRegistry};
use std::path::Path;

fn main() {
    let repo = Path::new("/path/to/repo");

    // Registry with built-ins plus your own runner.
    let mut registry = RunnerRegistry::with_builtins();
    registry.register(Box::new(MyRunner));

    let config = local_ci::load_config(repo)
        .expect("parse error")
        .expect("no config file");

    let mut all_ok = true;
    for job in &config.jobs {
        let result = local_ci::run_job_with(&registry, repo, job);
        println!("{}: {}", result.name, if result.ok { "PASS" } else { "FAIL" });
        all_ok &= result.ok;
    }
    std::process::exit(if all_ok { 0 } else { 1 });
}
```

Useful entry points:

| Function | Purpose |
|----------|---------|
| `load_config(root)` | Parse `.git-manage-ci.toml` (Ok(None) when absent) |
| `run_job(root, &job)` | One job, built-in runners |
| `run_job_with(&registry, root, &job)` | One job, custom registry |
| `run_all_cli(root)` | All jobs, prints results (what `devdock ci` uses) |
| `load_secrets(root)` | The `.git-manage-ci.secrets` map |
| `install_pre_push_hook(root)` | Write the git hook |

Jobs are independent, so parallelize with plain threads if you want; the
DevDock app runs each job on its own worker thread.

## Extending the config

`Config` and `Job` are plain serde types. To add fields:

1. Add the field with `#[serde(default)]` so existing configs keep parsing.
2. Handle it in the orchestration layer (`run_job_with`) if it affects all
   runners, or read it from your runner via `runner_target` conventions if
   it's environment-specific.
3. Add a parse test in `mod.rs` (see `parses_config_with_and_without_image`).

Keep secrets **out** of new config fields; that's what the secrets file and
`secrets = [...]` list are for.

## Testing your runner

The built-in tests show the pattern (see `runner.rs`):

```rust
#[test]
fn my_runner_executes() {
    let tmp = tempfile::tempdir().unwrap();
    let out = MyRunner
        .exec(&ExecRequest {
            repo_root: tmp.path(),
            script: "echo hello",
            env: &[("KEY".into(), "value".into())],
            target: Some("my-target"),
        })
        .unwrap();
    assert!(out.success);
    assert!(out.stdout.contains("hello"));
}
```

Test through `run_job_with` too, so secrets resolution and error paths are
covered:

```rust
let mut registry = RunnerRegistry::new();
registry.register(Box::new(MyRunner));
let job = /* Job with runner: Some("my-id") */;
let result = run_job_with(&registry, repo_root, &job);
```

## Design contract

Things the orchestration layer guarantees to every runner:

- `env` already contains resolved secrets; a job with missing secrets
  **never reaches your runner**.
- `script` uses `&&` semantics; don't re-split commands.
- Output is capped (64 KB) and timed by the caller; just return everything.

Things expected *hbhj*from** your runner:

- `id()` is stable and lowercase; it's a public config API.
- `available()` is cheap; it runs before every job.
- `exec` blocks until done; no async, no detached processes left behind.
- Non-zero exits are `Ok(ExecOutput { success: false, .. })`, not `Err`.
  `Err` is for infrastructure failures (cannot reach the environment).
