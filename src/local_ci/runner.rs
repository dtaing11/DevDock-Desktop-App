//! Extensible job runners for local CI.
//!
//! A [`Runner`] decides *where* a job's commands execute: on the host shell,
//! inside a Docker container, or anywhere a custom implementation puts them
//! (SSH box, Podman, a VM, a remote builder, ...).
//!
//! # Building your own runner
//!
//! Implement [`Runner`] and register it with [`RunnerRegistry::register`]:
//!
//! ```no_run
//! use git_manage::local_ci::runner::{ExecRequest, ExecOutput, Runner, RunnerRegistry};
//!
//! /// Runs jobs on a remote machine over SSH.
//! struct SshRunner;
//!
//! impl Runner for SshRunner {
//!     fn id(&self) -> &'static str {
//!         "ssh"
//!     }
//!
//!     fn available(&self) -> Result<(), String> {
//!         Ok(()) // e.g. check `ssh` exists and the host is reachable
//!     }
//!
//!     fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
//!         // request.target is the job's `runner_target`, e.g. "user@host".
//!         let target = request.target.ok_or("ssh runner needs runner_target")?;
//!         let output = std::process::Command::new("ssh")
//!             .arg(target)
//!             .arg(request.script)
//!             .output()
//!             .map_err(|e| e.to_string())?;
//!         Ok(ExecOutput {
//!             success: output.status.success(),
//!             stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
//!             stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
//!         })
//!     }
//! }
//!
//! let mut registry = RunnerRegistry::with_builtins();
//! registry.register(Box::new(SshRunner));
//! ```
//!
//! Jobs select a runner in `.git-manage-ci.toml` via `runner = "ssh"` and
//! pass runner-specific configuration through `runner_target`:
//!
//! ```toml
//! [[job]]
//! name = "tests on build box"
//! runner = "ssh"
//! runner_target = "builder@10.0.0.5"
//! commands = ["cd /srv/app && cargo test"]
//! ```
//!
//! The built-in runners are [`HostRunner`] (`runner` omitted or `"host"`)
//! and [`DockerRunner`] (`runner = "docker"`, or implied by `image = ...`).

use std::path::Path;
use std::process::Command;

/// Everything a runner needs to execute one job.
pub struct ExecRequest<'a> {
    /// Repository worktree root.
    pub repo_root: &'a Path,
    /// The job's commands joined with `&&` (stop at first failure).
    pub script: &'a str,
    /// Environment variables (config `env` plus resolved secrets).
    pub env: &'a [(String, String)],
    /// Runner-specific target: Docker image, SSH host, etc.
    pub target: Option<&'a str>,
}

/// What a runner produced.
pub struct ExecOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Where and how job commands execute.
///
/// Implementations must be `Send + Sync`: jobs run on worker threads.
pub trait Runner: Send + Sync {
    /// Identifier jobs reference via `runner = "<id>"` in the config.
    fn id(&self) -> &'static str;

    /// Checks prerequisites (binaries installed, daemon reachable).
    /// Called before `exec`; an `Err` fails the job with this message.
    fn available(&self) -> Result<(), String>;

    /// Executes the script and reports the outcome.
    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String>;
}

// ---------------------------------------------------------------------------
// Built-in: host shell
// ---------------------------------------------------------------------------

/// Runs commands directly on this machine's `sh`.
pub struct HostRunner;

impl Runner for HostRunner {
    fn id(&self) -> &'static str {
        "host"
    }

    fn available(&self) -> Result<(), String> {
        Ok(())
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", request.script]).current_dir(request.repo_root);
        for (key, value) in request.env {
            cmd.env(key, value);
        }
        let out = cmd.output().map_err(|e| format!("failed to start: {e}"))?;
        Ok(ExecOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Built-in: Docker
// ---------------------------------------------------------------------------

/// Runs commands inside a Docker container (Linux), repo mounted at `/work`.
pub struct DockerRunner;

impl Runner for DockerRunner {
    fn id(&self) -> &'static str {
        "docker"
    }

    fn available(&self) -> Result<(), String> {
        let ok = Command::new("docker")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err("Docker is not available. Install Docker or use the host runner.".into())
        }
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        let image = request
            .target
            .ok_or("docker runner needs an image (set `image = \"...\"` on the job)")?;
        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm", "-v"])
            .arg(format!("{}:/work", request.repo_root.display()))
            .args(["-w", "/work"]);
        for (key, value) in request.env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        cmd.arg(image).args(["sh", "-c", request.script]);
        let out = cmd.output().map_err(|e| format!("failed to start docker: {e}"))?;
        Ok(ExecOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Maps runner ids to implementations. Extend with [`Self::register`].
pub struct RunnerRegistry {
    runners: Vec<Box<dyn Runner>>,
}

impl RunnerRegistry {
    /// Empty registry (no runners, not even built-ins).
    pub fn new() -> Self {
        Self { runners: Vec::new() }
    }

    /// Registry with [`HostRunner`] and [`DockerRunner`].
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(HostRunner));
        registry.register(Box::new(DockerRunner));
        registry
    }

    /// Adds a runner. A runner with the same id replaces the earlier one,
    /// so custom implementations can override built-ins.
    pub fn register(&mut self, runner: Box<dyn Runner>) {
        self.runners.retain(|r| r.id() != runner.id());
        self.runners.push(runner);
    }

    /// Looks up a runner by id.
    pub fn get(&self, id: &str) -> Option<&dyn Runner> {
        self.runners.iter().find(|r| r.id() == id).map(|r| r.as_ref())
    }

    /// Registered runner ids, for error messages and UIs.
    pub fn ids(&self) -> Vec<&'static str> {
        self.runners.iter().map(|r| r.id()).collect()
    }
}

impl Default for RunnerRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRunner {
        ok: bool,
    }

    impl Runner for FakeRunner {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn available(&self) -> Result<(), String> {
            if self.ok { Ok(()) } else { Err("fake unavailable".into()) }
        }
        fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
            Ok(ExecOutput {
                success: true,
                stdout: format!("fake ran: {}", request.script),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn registry_finds_builtins_and_custom() {
        let mut registry = RunnerRegistry::with_builtins();
        assert!(registry.get("host").is_some());
        assert!(registry.get("docker").is_some());
        assert!(registry.get("fake").is_none());

        registry.register(Box::new(FakeRunner { ok: true }));
        assert!(registry.get("fake").is_some());
        assert_eq!(registry.ids().len(), 3);
    }

    #[test]
    fn register_overrides_same_id() {
        let mut registry = RunnerRegistry::new();
        registry.register(Box::new(FakeRunner { ok: true }));
        registry.register(Box::new(FakeRunner { ok: false }));
        assert_eq!(registry.ids(), vec!["fake"]);
        assert!(registry.get("fake").unwrap().available().is_err());
    }

    #[test]
    fn host_runner_executes() {
        let tmp = tempfile::tempdir().unwrap();
        let out = HostRunner
            .exec(&ExecRequest {
                repo_root: tmp.path(),
                script: "echo from-host-runner",
                env: &[],
                target: None,
            })
            .unwrap();
        assert!(out.success);
        assert!(out.stdout.contains("from-host-runner"));
    }

    #[test]
    fn custom_runner_receives_script() {
        let runner = FakeRunner { ok: true };
        let tmp = tempfile::tempdir().unwrap();
        let out = runner
            .exec(&ExecRequest {
                repo_root: tmp.path(),
                script: "anything",
                env: &[],
                target: Some("some-target"),
            })
            .unwrap();
        assert!(out.stdout.contains("anything"));
    }
}
