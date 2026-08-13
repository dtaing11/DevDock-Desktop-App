//! Local CI: runs user-configured checks before a pull request, similar in
//! spirit to GitHub Actions but on the developer's machine.
//!
//! Jobs are defined per-repository in `.git-manage-ci.toml` at the worktree
//! root:
//!
//! ```toml
//! [[job]]
//! name = "tests"
//! commands = ["cargo test"]
//!
//! [[job]]
//! name = "lint (ubuntu container)"
//! image = "rust:1.80"          # optional: run inside Docker
//! commands = ["cargo clippy -- -D warnings"]
//! ```
//!
//! When `image` is set the job runs in that Docker container with the
//! repository mounted at `/work`; otherwise commands run directly on the
//! host shell. Docker containers are Linux environments; for macOS/Windows
//! runners use hosted CI, since Docker cannot emulate those OSes.

pub mod runner;

pub use runner::{DockerRunner, ExecOutput, ExecRequest, HostRunner, Runner, RunnerRegistry};

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

/// Config file name at the repository root.
pub const CONFIG_FILE: &str = ".git-manage-ci.toml";

/// Local, non-committed secrets file at the repository root.
/// Simple KEY=VALUE lines; add it to .gitignore.
pub const SECRETS_FILE: &str = ".git-manage-ci.secrets";

/// Errors from config parsing or job execution.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CiError(pub String);

pub type Result<T> = std::result::Result<T, CiError>;

/// One configured check.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Job {
    pub name: String,
    /// Shell commands, run in order; the job fails on the first non-zero exit.
    pub commands: Vec<String>,
    /// Optional Docker image; when set the job runs inside the container.
    #[serde(default)]
    pub image: Option<String>,
    /// Extra environment variables (committed with the config; no secrets).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Names of secrets this job needs, loaded from [`SECRETS_FILE`] or the
    /// host environment. Values never live in the committed config.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Runner id: "host" (default), "docker", or a custom registered runner.
    /// When omitted, `image = ...` implies "docker".
    #[serde(default)]
    pub runner: Option<String>,
    /// Runner-specific target (e.g. SSH host). For Docker, `image` is used.
    #[serde(default)]
    pub runner_target: Option<String>,
}

impl Job {
    /// Effective runner id: explicit `runner`, else "docker" when an image
    /// is set, else "host".
    pub fn runner_id(&self) -> &str {
        match (&self.runner, &self.image) {
            (Some(id), _) => id,
            (None, Some(_)) => "docker",
            (None, None) => "host",
        }
    }

    /// Effective runner target: `runner_target`, falling back to `image`.
    pub fn target(&self) -> Option<&str> {
        self.runner_target.as_deref().or(self.image.as_deref())
    }
}

/// The parsed config file.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
    /// Push integration settings.
    #[serde(default)]
    pub on_push: OnPush,
}

/// How local CI hooks into pushing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OnPush {
    /// Run all jobs automatically before every push from the app.
    #[serde(default)]
    pub run: bool,
    /// When true, a failing job cancels the push. When false, failures
    /// only warn and the push proceeds.
    #[serde(default = "default_true")]
    pub block_on_failure: bool,
}

fn default_true() -> bool {
    true
}

impl Default for OnPush {
    fn default() -> Self {
        Self { run: false, block_on_failure: true }
    }
}

/// Outcome of one job run.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub name: String,
    pub ok: bool,
    /// Combined stdout+stderr, truncated to a sane size.
    pub output: String,
    pub duration_secs: f32,
}

/// Loads the CI config for a repository, if present.
pub fn load_config(repo_root: &Path) -> Result<Option<Config>> {
    let path = repo_root.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| CiError(e.to_string()))?;
    let config: Config =
        toml::from_str(&text).map_err(|e| CiError(format!("Invalid {CONFIG_FILE}: {e}")))?;
    Ok(Some(config))
}

/// Writes a starter config with commented examples.
pub fn write_template(repo_root: &Path) -> Result<()> {
    let template = r#"# Local CI for Git Manage: jobs run before creating a pull request.
# Jobs without `image` run on this machine. Jobs with `image` run inside
# that Docker container (Linux) with the repository mounted at /work.

# Run all jobs automatically before every push from the app:
# [on_push]
# run = true
# block_on_failure = true   # failing jobs cancel the push

[[job]]
name = "example"
commands = ["echo hello from local CI"]

# [[job]]
# name = "tests in container"
# image = "rust:latest"
# commands = ["cargo test"]

# [[job]]
# name = "integration tests with a secret"
# commands = ["./scripts/integration.sh"]
# secrets = ["API_TOKEN"]        # values come from .git-manage-ci.secrets
"#;
    std::fs::write(repo_root.join(CONFIG_FILE), template).map_err(|e| CiError(e.to_string()))
}

/// Whether the Docker CLI is available on this machine.
pub fn docker_available() -> bool {
    DockerRunner.available().is_ok()
}

/// Loads secrets from [`SECRETS_FILE`] (KEY=VALUE lines, `#` comments).
pub fn load_secrets(repo_root: &Path) -> std::collections::HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(repo_root.join(SECRETS_FILE)) else {
        return Default::default();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Resolves a job's secrets from the secrets file, falling back to the
/// host environment. Returns an error naming any missing secret.
fn resolve_secrets(
    repo_root: &Path,
    job: &Job,
) -> std::result::Result<Vec<(String, String)>, String> {
    let file_secrets = load_secrets(repo_root);
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for name in &job.secrets {
        match file_secrets.get(name).cloned().or_else(|| std::env::var(name).ok()) {
            Some(value) => resolved.push((name.clone(), value)),
            None => missing.push(name.clone()),
        }
    }
    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(format!(
            "Missing secret(s): {}. Add them to {SECRETS_FILE} (KEY=VALUE) or export them.",
            missing.join(", ")
        ))
    }
}

const MAX_OUTPUT: usize = 64 * 1024;

/// Runs one job with the default (built-in) runners.
pub fn run_job(repo_root: &Path, job: &Job) -> JobResult {
    run_job_with(&RunnerRegistry::with_builtins(), repo_root, job)
}

/// Runs one job using `registry` to resolve its runner. Blocking; call from
/// a worker thread. Custom tools embed their own registry here.
pub fn run_job_with(registry: &RunnerRegistry, repo_root: &Path, job: &Job) -> JobResult {
    let started = Instant::now();
    let fail = |output: String| JobResult {
        name: job.name.clone(),
        ok: false,
        output,
        duration_secs: started.elapsed().as_secs_f32(),
    };

    // 1. Resolve the runner.
    let runner_id = job.runner_id();
    let Some(runner) = registry.get(runner_id) else {
        return fail(format!(
            "Unknown runner \"{runner_id}\". Available: {}",
            registry.ids().join(", ")
        ));
    };
    if let Err(message) = runner.available() {
        return fail(message);
    }

    // 2. Resolve environment (config env + secrets).
    let mut env: Vec<(String, String)> =
        job.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    match resolve_secrets(repo_root, job) {
        Ok(secrets) => env.extend(secrets),
        Err(message) => return fail(message),
    }

    // 3. Execute.
    let script = job.commands.join(" && ");
    let request = ExecRequest { repo_root, script: &script, env: &env, target: job.target() };
    match runner.exec(&request) {
        Ok(out) => {
            let mut text = out.stdout;
            if !out.stderr.trim().is_empty() {
                text.push_str("\n--- stderr ---\n");
                text.push_str(&out.stderr);
            }
            if text.len() > MAX_OUTPUT {
                let mut end = MAX_OUTPUT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("\n[output truncated]");
            }
            JobResult {
                name: job.name.clone(),
                ok: out.success,
                output: text.trim().to_string(),
                duration_secs: started.elapsed().as_secs_f32(),
            }
        }
        Err(message) => fail(message),
    }
}

/// Runs all configured jobs headlessly, printing results to stdout.
/// Returns `true` when everything passed. Used by the `ci` CLI subcommand
/// (which the git pre-push hook invokes).
pub fn run_all_cli(repo_root: &Path) -> Result<bool> {
    let Some(config) = load_config(repo_root)? else {
        println!("devdock ci: no {CONFIG_FILE} found, nothing to run");
        return Ok(true);
    };
    if config.jobs.is_empty() {
        println!("devdock ci: no jobs configured");
        return Ok(true);
    }
    let mut all_ok = true;
    for job in &config.jobs {
        print!("devdock ci: {} ... ", job.name);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let result = run_job(repo_root, job);
        if result.ok {
            println!(
                "{} {}",
                crate::cli_style::status(true),
                crate::cli_style::dim(&format!("({:.1}s)", result.duration_secs))
            );
        } else {
            all_ok = false;
            println!(
                "{} {}",
                crate::cli_style::status(false),
                crate::cli_style::dim(&format!("({:.1}s)", result.duration_secs))
            );
            for line in result.output.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(all_ok)
}

/// Installs a git `pre-push` hook that runs the local CI via this binary.
/// Overwrites only hooks previously written by DevDock.
pub fn install_pre_push_hook(repo_root: &Path) -> Result<()> {
    let hook_dir = repo_root.join(".git").join("hooks");
    std::fs::create_dir_all(&hook_dir).map_err(|e| CiError(e.to_string()))?;
    let hook_path = hook_dir.join("pre-push");

    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
        if !existing.contains("devdock-local-ci") {
            return Err(CiError(
                "A pre-push hook already exists (not created by DevDock). \
                 Merge manually or remove .git/hooks/pre-push first."
                    .into(),
            ));
        }
    }

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "devdock".into());
    let script = format!(
        "#!/bin/sh\n\
         # devdock-local-ci: runs .git-manage-ci.toml jobs before every push.\n\
         # Regenerate from the DevDock app; delete this file to disable.\n\
         exec \"{exe}\" ci\n"
    );
    std::fs::write(&hook_path, script).map_err(|e| CiError(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| CiError(e.to_string()))?;
    }
    Ok(())
}

/// Whether the DevDock pre-push hook is installed in this repository.
pub fn hook_installed(repo_root: &Path) -> bool {
    std::fs::read_to_string(repo_root.join(".git").join("hooks").join("pre-push"))
        .map(|s| s.contains("devdock-local-ci"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_with_and_without_image() {
        let text = r#"
[[job]]
name = "local"
commands = ["echo hi"]

[[job]]
name = "container"
image = "alpine:3"
commands = ["uname -a"]
env = { FOO = "bar" }
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.jobs.len(), 2);
        assert_eq!(config.jobs[0].name, "local");
        assert!(config.jobs[0].image.is_none());
        assert_eq!(config.jobs[1].image.as_deref(), Some("alpine:3"));
        assert_eq!(config.jobs[1].env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn runs_passing_and_failing_host_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let pass = Job {
            name: "pass".into(),
            commands: vec!["echo output-here".into()],
            image: None,
            env: Default::default(),
            secrets: Vec::new(),
            runner: None,
            runner_target: None,
        };
        let result = run_job(tmp.path(), &pass);
        assert!(result.ok);
        assert!(result.output.contains("output-here"));

        let fail = Job {
            name: "fail".into(),
            commands: vec!["echo before".into(), "false".into(), "echo after".into()],
            image: None,
            env: Default::default(),
            secrets: Vec::new(),
            runner: None,
            runner_target: None,
        };
        let result = run_job(tmp.path(), &fail);
        assert!(!result.ok);
        assert!(result.output.contains("before"));
        assert!(!result.output.contains("after"), "must stop at first failure");
    }

    #[test]
    fn env_vars_reach_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("MY_VAR".to_string(), "custom-value".to_string());
        let job =
            Job {
                name: "env".into(),
                commands: vec!["echo $MY_VAR".into()],
                image: None,
                env,
                secrets: Vec::new(),
                runner: None,
                runner_target: None,
            };
        let result = run_job(tmp.path(), &job);
        assert!(result.ok);
        assert!(result.output.contains("custom-value"));
    }

    #[test]
    fn template_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path()).unwrap();
        let config = load_config(tmp.path()).unwrap().expect("config written");
        assert_eq!(config.jobs.len(), 1);
        assert_eq!(config.jobs[0].name, "example");
    }

    #[test]
    fn secrets_from_file_reach_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(SECRETS_FILE),
            "# comment line\nAPI_TOKEN = sekrit-123\n",
        )
        .unwrap();
        let job = Job {
            name: "secret".into(),
            commands: vec!["echo token=$API_TOKEN".into()],
            image: None,
            env: Default::default(),
            secrets: vec!["API_TOKEN".into()],
            runner: None,
            runner_target: None,
        };
        let result = run_job(tmp.path(), &job);
        assert!(result.ok, "{}", result.output);
        assert!(result.output.contains("token=sekrit-123"));
    }

    #[test]
    fn missing_secret_fails_with_clear_message() {
        let tmp = tempfile::tempdir().unwrap();
        let job = Job {
            name: "no-secret".into(),
            commands: vec!["echo hi".into()],
            image: None,
            env: Default::default(),
            secrets: vec!["DEFINITELY_NOT_SET_ANYWHERE_XYZ".into()],
            runner: None,
            runner_target: None,
        };
        let result = run_job(tmp.path(), &job);
        assert!(!result.ok);
        assert!(result.output.contains("DEFINITELY_NOT_SET_ANYWHERE_XYZ"));
        assert!(result.output.contains(SECRETS_FILE));
    }

    #[test]
    fn missing_config_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_config(tmp.path()).unwrap().is_none());
    }
}
