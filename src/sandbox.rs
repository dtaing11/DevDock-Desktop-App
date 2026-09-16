//! A machine of the run's own: where an unattended agent's commands and
//! the repository's checks execute when the developer wants them off the
//! host. Not a padded cell — the network is on, the agent has a full shell
//! with root, and what it installs stays installed for the next run.
//!
//! Three runtimes, whichever is on the machine:
//!
//! - **Lima** (`limactl`): a Linux virtual machine, kept between runs, with
//!   the home directory mounted writable at the same path, so a worktree is
//!   where the agent expects it. Toolchains it installs live in the VM.
//! - **Apple's `container`** (macOS 26): a Linux container in its own
//!   lightweight VM; `docker`-shaped commands.
//! - **Docker**: a long-lived container per run rather than one per command,
//!   so `apt install` in one command is there for the next.
//!
//! The container runtimes mount the worktree at `/work` and a named volume,
//! `devdock-toolchains`, at `/opt/devdock`, with `HOME` inside it: an SDK
//! installed under `~` persists across runs and can be cleared with one
//! `volume rm`. The base image is plain Ubuntu unless the developer names
//! another; the agent installs what the repository needs.
//!
//! One [`Sandbox`] is one run: [`Sandbox::start`] at the top, every command
//! through [`Sandbox::exec`], and the container removed when it is dropped.
//! The Lima VM is left running — starting one takes a while — and the
//! worktree it was working in is removed by the caller like any other.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::local_ci::runner::{ExecOutput, ExecRequest, Runner};

/// Which runtime hosts the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lima,
    AppleContainer,
    Docker,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Lima, Kind::AppleContainer, Kind::Docker];

    /// The name a setting or a flag uses.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Lima => "lima",
            Kind::AppleContainer => "container",
            Kind::Docker => "docker",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Lima => "Lima VM",
            Kind::AppleContainer => "Apple container",
            Kind::Docker => "Docker",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        match text.trim().to_ascii_lowercase().as_str() {
            "lima" | "vm" => Some(Kind::Lima),
            "container" | "apple" | "apple-container" => Some(Kind::AppleContainer),
            "docker" => Some(Kind::Docker),
            _ => None,
        }
    }

    /// Whether the runtime's command is on this machine (not whether it is
    /// up: that is checked, and said, when a run starts).
    pub fn installed(self) -> bool {
        let program = match self {
            Kind::Lima => "limactl",
            Kind::AppleContainer => "container",
            Kind::Docker => "docker",
        };
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Whether the runtime takes an image name.
    pub fn uses_image(self) -> bool {
        !matches!(self, Kind::Lima)
    }
}

/// The runtimes on this machine, in the order they are preferred: a VM
/// first, because it needs no daemon and keeps everything.
pub fn installed() -> Vec<Kind> {
    Kind::ALL.into_iter().filter(|k| k.installed()).collect()
}

/// What the developer asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spec {
    /// A runtime, or whichever is installed.
    pub kind: Option<Kind>,
    /// Image for a container runtime; empty means [`DEFAULT_IMAGE`]. Lima
    /// ignores it — its VM is Ubuntu.
    pub image: String,
}

/// A base with a package manager and nothing else; the agent adds to it.
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";
/// The Lima instance every run shares.
pub const LIMA_INSTANCE: &str = "devdock";
/// The volume container runtimes keep toolchains in.
pub const TOOLCHAIN_VOLUME: &str = "devdock-toolchains";
const MOUNT: &str = "/work";
const TOOLCHAIN_MOUNT: &str = "/opt/devdock";

impl Spec {
    /// From a `--sandbox` value: a runtime's name, or an image for
    /// whichever container runtime is installed.
    pub fn parse(text: &str) -> Spec {
        match Kind::parse(text) {
            Some(kind) => Spec { kind: Some(kind), image: String::new() },
            None => Spec { kind: None, image: text.trim().to_string() },
        }
    }

    fn image(&self) -> &str {
        let image = self.image.trim();
        if image.is_empty() { DEFAULT_IMAGE } else { image }
    }
}

/// A running sandbox for one run.
#[derive(Debug)]
pub struct Sandbox {
    kind: Kind,
    /// Container name, or the Lima instance.
    name: String,
    /// The worktree on the host.
    root: PathBuf,
    /// Where the worktree is inside: `/work`, or the same path in a VM.
    inner_root: String,
    image: String,
}

fn container_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("devdock-sandbox-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

impl Sandbox {
    /// Starts a sandbox for the worktree at `root`, saying what it did.
    pub fn start(spec: &Spec, root: &Path, log: &mut dyn FnMut(String)) -> Result<Sandbox, String> {
        let kind = match spec.kind {
            Some(kind) if kind.installed() => kind,
            Some(kind) => {
                return Err(format!(
                    "the sandbox is set to {}, but `{}` is not installed on this machine",
                    kind.label(),
                    match kind { Kind::Lima => "limactl", Kind::AppleContainer => "container", Kind::Docker => "docker" }
                ))
            }
            None => installed().into_iter().next().ok_or(
                "no sandbox runtime is installed. Install Lima (`brew install lima`) for a \
                 Linux VM, Apple's `container` on macOS 26, or Docker.",
            )?,
        };
        let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
        match kind {
            Kind::Lima => Self::start_lima(root, log),
            Kind::Docker => Self::start_container("docker", Kind::Docker, spec.image(), root, log),
            Kind::AppleContainer => Self::start_container("container", Kind::AppleContainer, spec.image(), root, log),
        }
    }

    fn start_lima(root: PathBuf, log: &mut dyn FnMut(String)) -> Result<Sandbox, String> {
        let list = run("limactl", &["list", "--format", "{{.Name}} {{.Status}}"])?;
        let status = list
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{LIMA_INSTANCE} ")))
            .map(|s| s.trim().to_string());
        match status.as_deref() {
            Some("Running") => {}
            Some(_) => {
                log(format!("starting the {LIMA_INSTANCE} VM"));
                run("limactl", &["start", LIMA_INSTANCE, "--tty=false"])?;
            }
            None => {
                log(format!("creating the {LIMA_INSTANCE} VM (Ubuntu; the first time downloads an image)"));
                // Home writable, and the temp trees a worktree may be in.
                run(
                    "limactl",
                    &[
                        "create",
                        &format!("--name={LIMA_INSTANCE}"),
                        "--tty=false",
                        "--set",
                        ".mounts[0].writable = true | .mounts += [{\"location\":\"/private/tmp\",\"writable\":true},{\"location\":\"/private/var/folders\",\"writable\":true}]",
                        "template:default",
                    ],
                )?;
                run("limactl", &["start", LIMA_INSTANCE, "--tty=false"])?;
            }
        }
        let inner_root = root.display().to_string();
        // The worktree must be reachable inside, or nothing else matters.
        let probe = run("limactl", &["shell", "--workdir", &inner_root, LIMA_INSTANCE, "sh", "-c", "test -w ."]);
        if let Err(e) = probe {
            return Err(format!(
                "the {LIMA_INSTANCE} VM cannot write to {inner_root}: it mounts your home \
                 directory; put the repository under it, or recreate the VM with that path \
                 mounted (`limactl delete {LIMA_INSTANCE}` and run again). {e}"
            ));
        }
        log(format!("sandbox: Lima VM {LIMA_INSTANCE}, network on, worktree at {inner_root}; what it installs stays"));
        Ok(Sandbox { kind: Kind::Lima, name: LIMA_INSTANCE.into(), root, inner_root, image: String::new() })
    }

    fn start_container(program: &str, kind: Kind, image: &str, root: PathBuf, log: &mut dyn FnMut(String)) -> Result<Sandbox, String> {
        let name = container_name();
        let mount = format!("{}:{MOUNT}", root.display());
        let volume = format!("{TOOLCHAIN_VOLUME}:{TOOLCHAIN_MOUNT}");
        let home = format!("HOME={TOOLCHAIN_MOUNT}/home");
        run(
            program,
            &[
                "run", "-d", "--init", "--name", &name, "-v", &mount, "-v", &volume, "-e", &home, "-w", MOUNT, image,
                "sh", "-c", &format!("mkdir -p {TOOLCHAIN_MOUNT}/home && exec sleep infinity"),
            ],
        )
        .map_err(|e| format!("could not start the {} sandbox from {image}: {e}", kind.label()))?;
        log(format!(
            "sandbox: {} {image}, network on, worktree at {MOUNT}; toolchains persist in the {TOOLCHAIN_VOLUME} volume",
            kind.label()
        ));
        Ok(Sandbox { kind, name, root, inner_root: MOUNT.into(), image: image.to_string() })
    }

    /// One line for logs and pull requests.
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::Lima => format!("Lima VM {}", self.name),
            other => format!("{} ({})", other.label(), self.image),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The worktree's path inside the sandbox.
    pub fn inner_root(&self) -> &str {
        &self.inner_root
    }

    /// Runs `script` with `sh -lc` in `subdir` of the worktree, inside.
    pub fn exec(&self, script: &str, subdir: &str, env: &[(String, String)], timeout: Option<Duration>) -> Result<ExecOutput, String> {
        let workdir = if subdir.is_empty() { self.inner_root.clone() } else { format!("{}/{subdir}", self.inner_root) };
        let mut cmd = match self.kind {
            Kind::Lima => {
                let mut c = Command::new("limactl");
                c.args(["shell", "--workdir", &workdir, &self.name]);
                // Environment goes in the script: ssh does not carry it.
                let mut prefix = String::new();
                for (k, v) in env {
                    prefix.push_str(&format!("export {k}={}; ", shell_quote(v)));
                }
                c.args(["sh", "-lc", &format!("{prefix}{script}")]);
                c
            }
            Kind::Docker | Kind::AppleContainer => {
                let mut c = Command::new(if self.kind == Kind::Docker { "docker" } else { "container" });
                c.args(["exec", "-w", &workdir]);
                for (k, v) in env {
                    c.arg("-e").arg(format!("{k}={v}"));
                }
                c.arg(&self.name).args(["sh", "-lc", script]);
                c
            }
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let child = cmd.spawn().map_err(|e| format!("could not start the sandbox command: {e}"))?;
        let pid = child.id();
        crate::local_ci::runner::wait_with_timeout(child, timeout, || crate::local_ci::runner::kill_group(pid))
    }

    /// The host path of the worktree this sandbox is over.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        match self.kind {
            // The VM stays: starting it is slow and what it holds is the point.
            Kind::Lima => {}
            Kind::Docker => {
                let _ = Command::new("docker").args(["rm", "-f", &self.name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            Kind::AppleContainer => {
                let _ = Command::new("container").args(["rm", "-f", &self.name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The sandbox as a check runner: jobs with `runner = "sandbox"` execute
/// inside it, so DevDock's own verification and the agent's commands run
/// in the same place.
pub struct SandboxRunner(pub std::sync::Arc<Sandbox>);

/// The runner id jobs are pointed at.
pub const RUNNER_ID: &str = "sandbox";

impl Runner for SandboxRunner {
    fn id(&self) -> &'static str {
        RUNNER_ID
    }

    fn available(&self) -> Result<(), String> {
        Ok(())
    }

    fn exec(&self, request: &ExecRequest<'_>) -> Result<ExecOutput, String> {
        self.0.exec(request.script, request.work_subdir, request.env, request.timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_is_a_runtime_name_or_an_image() {
        assert_eq!(Spec::parse("lima"), Spec { kind: Some(Kind::Lima), image: String::new() });
        assert_eq!(Spec::parse("Docker"), Spec { kind: Some(Kind::Docker), image: String::new() });
        assert_eq!(Spec::parse("rust:1-bookworm"), Spec { kind: None, image: "rust:1-bookworm".into() });
        assert_eq!(Spec::default().image(), DEFAULT_IMAGE);
        assert_eq!(Kind::parse("nope"), None);
        assert!(!Kind::Lima.uses_image() && Kind::Docker.uses_image());
    }

    #[test]
    fn a_runtime_that_is_not_installed_is_refused_with_its_name() {
        // Whatever this machine has, an impossible request says so.
        let missing = Kind::ALL.into_iter().find(|k| !k.installed());
        let Some(kind) = missing else {
            eprintln!("every runtime is installed here; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let err = Sandbox::start(&Spec { kind: Some(kind), image: String::new() }, tmp.path(), &mut |_| {}).unwrap_err();
        assert!(err.contains(kind.label()) && err.contains("not installed"), "{err}");
    }

    /// A real sandbox on whatever runtime is installed: the network is on,
    /// a command runs in the worktree, root is available, and something
    /// installed in one command is there for the next.
    /// `cargo test --lib sandbox::tests::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_a_sandbox_has_the_network_the_worktree_and_a_memory() {
        if installed().is_empty() {
            eprintln!("no runtime installed; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hi\n").unwrap();
        let mut log = Vec::new();
        let sandbox = Sandbox::start(&Spec::default(), tmp.path(), &mut |l| {
            println!("  {l}");
            log.push(l);
        })
        .unwrap();
        let out = sandbox.exec("cat hello.txt && uname -a && id -u", "", &[], Some(Duration::from_secs(60))).unwrap();
        println!("{}{}", out.stdout, out.stderr);
        assert!(out.success);
        assert!(out.stdout.contains("hi\n") && out.stdout.contains("Linux"), "{}", out.stdout);
        let out = sandbox.exec("echo made-inside > from-inside.txt", "", &[], Some(Duration::from_secs(60))).unwrap();
        assert!(out.success, "{}", out.stderr);
        assert_eq!(std::fs::read_to_string(tmp.path().join("from-inside.txt")).unwrap(), "made-inside\n", "writes reach the host");
        let out = sandbox.exec("curl -fsSI https://example.com | head -1 || wget -qS --spider https://example.com 2>&1 | head -1", "", &[], Some(Duration::from_secs(60))).unwrap();
        assert!(out.success && (out.stdout.contains("200") || out.stderr.contains("200")), "network: {}{}", out.stdout, out.stderr);
        let out = sandbox.exec("echo remembered > ~/devdock-memory && cat ~/devdock-memory", "", &[], Some(Duration::from_secs(60))).unwrap();
        assert!(out.stdout.contains("remembered"), "{}{}", out.stdout, out.stderr);
        let out = sandbox.exec("X=$FOO; echo env=$X", "", &[("FOO".into(), "bar baz".into())], Some(Duration::from_secs(60))).unwrap();
        assert!(out.stdout.contains("env=bar baz"), "{}", out.stdout);
        let out = sandbox.exec("sleep 30", "", &[], Some(Duration::from_secs(2))).unwrap();
        assert!(!out.success, "a timeout is a failure");
        println!("describe: {}", sandbox.describe());
    }
}
