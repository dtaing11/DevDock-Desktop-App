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
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Everything a runner needs to execute one job.
pub struct ExecRequest<'a> {
    /// Repository worktree root. Stays the mount root for container runners so
    /// cross-package paths keep working in a monorepo.
    pub repo_root: &'a Path,
    /// Directory the commands run in, relative to [`Self::repo_root`]. Empty
    /// for a root-level job; set when the job came from a nested config.
    pub work_subdir: &'a str,
    /// The job's commands joined with `&&` (stop at first failure).
    pub script: &'a str,
    /// Environment variables (config `env` plus resolved secrets).
    pub env: &'a [(String, String)],
    /// Runner-specific target: Docker image, SSH host, etc.
    pub target: Option<&'a str>,
    /// How long the job may run before it is killed and reported as failed.
    /// `None` waits forever, which is right for nothing an agent starts.
    pub timeout: Option<Duration>,
}

impl ExecRequest<'_> {
    /// Absolute directory the commands should run in.
    pub fn workdir(&self) -> std::path::PathBuf {
        if self.work_subdir.is_empty() {
            self.repo_root.to_path_buf()
        } else {
            self.repo_root.join(self.work_subdir)
        }
    }

    /// Working directory inside a container, given the repo is mounted at
    /// `mount`.
    pub fn container_workdir(&self, mount: &str) -> String {
        if self.work_subdir.is_empty() {
            mount.to_string()
        } else {
            format!("{mount}/{}", self.work_subdir)
        }
    }
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
        cmd.args(["-c", request.script]).current_dir(request.workdir());
        // An app opened from the Finder or a launcher gets a bare PATH;
        // the checks need the one the developer's terminal has, where
        // flutter, cargo and node live.
        cmd.env("PATH", login_path());
        for (key, value) in request.env {
            cmd.env(key, value);
        }
        // Its own process group, so a timeout can take the whole tree —
        // the test runner and the server it started — not just the shell.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start: {e}"))?;
        let pid = child.id();
        wait_with_timeout(child, request.timeout, || kill_group(pid))
    }
}

/// The PATH the developer's login shell has, read once: this process's
/// own PATH is a bare default when the app was started from the Finder.
pub fn login_path() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let own = std::env::var("PATH").unwrap_or_default();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let from_shell = Command::new(&shell)
            .args(["-lc", "printf %s \"$PATH\""])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if from_shell.is_empty() {
            return own;
        }
        // The shell's first, then anything this process had that it lacks.
        let mut merged: Vec<&str> = from_shell.split(':').filter(|p| !p.is_empty()).collect();
        for p in own.split(':') {
            if !p.is_empty() && !merged.contains(&p) {
                merged.push(p);
            }
        }
        merged.join(":")
    })
    .clone()
}

/// Ends every process in `pid`'s group; on other platforms the child alone
/// is killed by the caller.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    // SAFETY: a plain signal to a process group this process created.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {}

// ---------------------------------------------------------------------------
// Built-in: Docker
// ---------------------------------------------------------------------------

/// Runs commands inside a Docker container (Linux), repo mounted at `/work`.
pub struct DockerRunner;

/// Outcome of running `docker info`, split so the two failures can be told
/// apart (and tested) without Docker installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockerProbe {
    /// The binary launched. `success` is whether it reached the daemon.
    Ran { success: bool },
    /// The binary could not be launched at all.
    NotLaunched,
}

/// Turns a probe into an availability verdict.
///
/// The daemon-down and not-installed cases have different fixes, so they get
/// different messages instead of one "Docker is not available".
fn classify_docker_probe(probe: DockerProbe) -> Result<(), String> {
    match probe {
        DockerProbe::Ran { success: true } => Ok(()),
        DockerProbe::Ran { success: false } => Err(
            "Docker is installed but its daemon is not running. Start Docker \
             Desktop (macOS/Windows) or `sudo systemctl start docker` (Linux), \
             then run the checks again. Jobs without `image` are unaffected."
                .into(),
        ),
        DockerProbe::NotLaunched => Err(
            "Docker is not installed, or `docker` is not on PATH. Install \
             Docker Desktop (or colima/podman-docker), or drop `image` from \
             the job to run it on this machine instead."
                .into(),
        ),
    }
}

impl Runner for DockerRunner {
    fn id(&self) -> &'static str {
        "docker"
    }

    /// Probes with `docker info`, which needs the daemon.
    ///
    /// `docker --version` is not a usable check: it only prints the client
    /// version and succeeds with the daemon stopped, so the job would run and
    /// fail on a raw "Cannot connect to the Docker daemon" from stderr instead
    /// of this message. The two cases also have different fixes, so they get
    /// different messages.
    fn available(&self) -> Result<(), String> {
        classify_docker_probe(match Command::new("docker").arg("info").output() {
            Ok(out) => DockerProbe::Ran { success: out.status.success() },
            Err(_) => DockerProbe::NotLaunched,
        })
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        let image = request
            .target
            .ok_or("docker runner needs an image (set `image = \"...\"` on the job)")?;
        // Named, so a timeout can remove the container by name: killing the
        // `docker run` client leaves the container running otherwise.
        let name = container_name();
        let mut cmd = Command::new("docker");
        // The whole repository is mounted, with the workdir pointing at the
        // job's own directory, so a nested job can still reach sibling
        // packages by relative path. `--rm` removes the container and its
        // anonymous volumes when it exits, so nothing accumulates between
        // runs; `--init` gives it a real PID 1, so signals reach the build.
        cmd.args(["run", "--rm", "--init", "--name", &name, "-v"])
            .arg(format!("{}:/work", request.repo_root.display()))
            .arg("-w")
            .arg(request.container_workdir("/work"));
        // Pass env var NAMES only in argv; docker reads the values from
        // this process's environment, keeping secrets out of `ps` output.
        for (key, value) in request.env {
            cmd.arg("-e").arg(key);
            cmd.env(key, value);
        }
        cmd.arg(image).args(["sh", "-c", request.script]);
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start docker: {e}"))?;
        wait_with_timeout(child, request.timeout, || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        })
    }
}

/// A container name unique to this process and call.
fn container_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("devdock-ci-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Waits for a child, collecting its output, and kills it — through
/// `teardown`, then directly — once `timeout` has passed. A timed-out job
/// is a failed job whose stderr says so.
pub fn wait_with_timeout(
    mut child: Child,
    timeout: Option<Duration>,
    teardown: impl FnOnce(),
) -> Result<ExecOutput, String> {
    // Both pipes drained on their own threads: a child that fills one
    // while this thread waits on the other deadlocks.
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    });
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(format!("waiting for the job: {e}")),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            teardown();
            let _ = child.kill();
            break child.wait().map_err(|e| format!("waiting for the job: {e}"))?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let collect = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
        handle
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let mut err = collect(stderr);
    if timed_out {
        let secs = timeout.map(|t| t.as_secs()).unwrap_or(0);
        err.push_str(&format!("\n[killed: the job ran longer than {secs}s]"));
    }
    Ok(ExecOutput {
        success: status.success() && !timed_out,
        stdout: collect(stdout),
        stderr: err,
    })
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


    /// `docker --version` succeeds with the daemon stopped, so it cannot be
    /// the availability probe: the job would run and fail on a raw "Cannot
    /// connect to the Docker daemon" instead of a message that says what to do.
    #[test]
    fn docker_probe_tells_daemon_down_apart_from_not_installed() {
        assert!(classify_docker_probe(DockerProbe::Ran { success: true }).is_ok());

        let daemon_down =
            classify_docker_probe(DockerProbe::Ran { success: false }).unwrap_err();
        assert!(daemon_down.contains("daemon is not running"), "got: {daemon_down}");
        assert!(daemon_down.contains("Start Docker"), "should say what to do: {daemon_down}");

        let missing = classify_docker_probe(DockerProbe::NotLaunched).unwrap_err();
        assert!(missing.contains("not installed"), "got: {missing}");

        // The two must not be the same message — they have different fixes.
        assert_ne!(daemon_down, missing);
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
                work_subdir: "",
                repo_root: tmp.path(),
                script: "echo from-host-runner",
                env: &[],
                target: None,
                timeout: None,
            })
            .unwrap();
        assert!(out.success);
        assert!(out.stdout.contains("from-host-runner"));
    }

    #[test]
    fn a_job_that_runs_too_long_is_killed_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = HostRunner
            .exec(&ExecRequest {
                work_subdir: "",
                repo_root: tmp.path(),
                script: "echo started; sleep 30; echo never",
                env: &[],
                target: None,
                timeout: Some(Duration::from_millis(400)),
            })
            .unwrap();
        assert!(!out.success);
        assert!(out.stdout.contains("started"), "{}", out.stdout);
        assert!(!out.stdout.contains("never"));
        assert!(out.stderr.contains("ran longer than"), "{}", out.stderr);
        assert!(started.elapsed() < Duration::from_secs(10), "the sleep was not killed");
    }

    #[test]
    fn custom_runner_receives_script() {
        let runner = FakeRunner { ok: true };
        let tmp = tempfile::tempdir().unwrap();
        let out = runner
            .exec(&ExecRequest {
                work_subdir: "",
                repo_root: tmp.path(),
                script: "anything",
                env: &[],
                target: Some("some-target"),
                timeout: None,
            })
            .unwrap();
        assert!(out.stdout.contains("anything"));
    }
}
