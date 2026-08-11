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

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Config file name at the repository root.
pub const CONFIG_FILE: &str = ".git-manage-ci.toml";

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
    /// Extra environment variables.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// The parsed config file.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
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

[[job]]
name = "example"
commands = ["echo hello from local CI"]

# [[job]]
# name = "tests in container"
# image = "rust:latest"
# commands = ["cargo test"]
"#;
    std::fs::write(repo_root.join(CONFIG_FILE), template).map_err(|e| CiError(e.to_string()))
}

/// Whether the Docker CLI is available on this machine.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const MAX_OUTPUT: usize = 64 * 1024;

/// Runs one job to completion (blocking; call from a worker thread).
pub fn run_job(repo_root: &Path, job: &Job) -> JobResult {
    let started = Instant::now();
    let script = job.commands.join(" && ");

    let output = if let Some(image) = &job.image {
        if !docker_available() {
            return JobResult {
                name: job.name.clone(),
                ok: false,
                output: "Docker is not available. Install Docker or remove `image` \
                         from this job to run it on the host."
                    .into(),
                duration_secs: 0.0,
            };
        }
        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm", "-v"])
            .arg(format!("{}:/work", repo_root.display()))
            .args(["-w", "/work"]);
        for (key, value) in &job.env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        cmd.arg(image).args(["sh", "-c", &script]);
        cmd.output()
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]).current_dir(repo_root);
        for (key, value) in &job.env {
            cmd.env(key, value);
        }
        cmd.output()
    };

    match output {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                text.push_str("\n--- stderr ---\n");
                text.push_str(&stderr);
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
                ok: out.status.success(),
                output: text.trim().to_string(),
                duration_secs: started.elapsed().as_secs_f32(),
            }
        }
        Err(e) => JobResult {
            name: job.name.clone(),
            ok: false,
            output: format!("failed to start: {e}"),
            duration_secs: started.elapsed().as_secs_f32(),
        },
    }
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
        };
        let result = run_job(tmp.path(), &pass);
        assert!(result.ok);
        assert!(result.output.contains("output-here"));

        let fail = Job {
            name: "fail".into(),
            commands: vec!["echo before".into(), "false".into(), "echo after".into()],
            image: None,
            env: Default::default(),
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
            Job { name: "env".into(), commands: vec!["echo $MY_VAR".into()], image: None, env };
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
    fn missing_config_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_config(tmp.path()).unwrap().is_none());
    }
}
