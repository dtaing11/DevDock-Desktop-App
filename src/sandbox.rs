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

    /// Installs what the repository's toolchains need and the sandbox does
    /// not have — Flutter for a `pubspec.yaml`, Rust for a `Cargo.toml`,
    /// Node, Python, Go — into the sandbox's home, where it stays for the
    /// next run. `programs` are the commands the checks and toolchains use.
    /// A plain Ubuntu image knows none of them; this is what makes it able
    /// to build and test the repository.
    pub fn provision(&self, programs: &[&str], log: &mut dyn FnMut(String)) -> Result<(), String> {
        let missing: Vec<&str> = {
            let probe = programs
                .iter()
                .map(|p| match recipe_for(p).and_then(|r| r.check) {
                    Some(check) => format!("( {check} ) >/dev/null 2>&1 || echo MISSING:{p}"),
                    None => format!("command -v {p} >/dev/null 2>&1 || echo MISSING:{p}"),
                })
                .collect::<Vec<_>>()
                .join("; ");
            if probe.is_empty() {
                return Ok(());
            }
            let out = self.exec(&probe, "", &[], Some(Duration::from_secs(60)))?;
            out.stdout.lines().filter_map(|l| l.strip_prefix("MISSING:")).map(|p| p.trim()).filter(|p| !p.is_empty()).map(|p| programs.iter().copied().find(|q| *q == p).unwrap_or("")).filter(|p| !p.is_empty()).collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        let mut done = std::collections::BTreeSet::new();
        for program in &missing {
            let Some(recipe) = recipe_for(program) else {
                log(format!("sandbox: no recipe to install `{program}`; the agent can install it itself"));
                continue;
            };
            if !done.insert(recipe.name) {
                continue;
            }
            log(format!("sandbox: installing {} (missing `{program}`; the first time takes a while, then it is kept)", recipe.name));
            let out = self.exec(recipe.script, "", &[], Some(Duration::from_secs(1_800)))?;
            if !out.success {
                let tail: String = out.stderr.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                return Err(format!("could not install {} in the sandbox:\n{tail}", recipe.name));
            }
            log(format!("sandbox: {} installed", recipe.name));
        }
        Ok(())
    }

    /// Gives the sandbox's Claude Code the developer's sign-in, once: the
    /// credentials Claude Code keeps in the macOS Keychain, written to the
    /// file it reads on Linux, in the sandbox's home (mode 600). Nothing is
    /// copied when the sandbox already has them. `Ok(true)` when seeded.
    ///
    /// Without a sign-in on this machine, the way in is a one-time
    /// `claude` login inside the sandbox, and the error says so.
    pub fn seed_claude_credentials(&self, log: &mut dyn FnMut(String)) -> Result<bool, String> {
        let probe = self.exec("test -s \"$HOME/.claude/.credentials.json\" && echo HAVE", "", &[], Some(Duration::from_secs(30)))?;
        if probe.stdout.contains("HAVE") {
            return Ok(false);
        }
        let json = host_claude_credentials().ok_or_else(|| {
            format!(
                "the sandbox's Claude Code is not signed in, and no Claude Code sign-in was found on \
                 this machine to copy. Sign in once inside it: `{}` then `/login`.",
                self.login_hint()
            )
        })?;
        let mut cmd = self.command(
            "sh",
            &["-c".to_string(), "mkdir -p \"$HOME/.claude\" && umask 077 && cat > \"$HOME/.claude/.credentials.json\"".to_string()],
            &Default::default(),
            "",
        );
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("could not seed credentials: {e}"))?;
        {
            use std::io::Write as _;
            let mut stdin = child.stdin.take().ok_or("no stdin")?;
            stdin.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("could not seed credentials: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        log("sandbox: Claude Code signed in with this machine's Claude Code credentials".into());
        Ok(true)
    }

    /// How to open a shell in the sandbox, for a message.
    fn login_hint(&self) -> String {
        match self.kind {
            Kind::Lima => format!("limactl shell {} claude", self.name),
            Kind::Docker => format!("docker exec -it {} claude", self.name),
            Kind::AppleContainer => format!("container exec -it {} claude", self.name),
        }
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

    /// Environment every command inside gets: a cargo target directory of
    /// the sandbox's own, per worktree, so a Linux build never lands in the
    /// host's `target/` and is there again next run.
    fn base_env(&self) -> Vec<(String, String)> {
        let slug: String = self.inner_root.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        vec![("CARGO_TARGET_DIR".into(), format!("$HOME/.devdock-cargo-target/{}", slug.trim_matches('-')))]
    }

    /// Runs `script` with `sh -lc` in `subdir` of the worktree, inside.
    pub fn exec(&self, script: &str, subdir: &str, env: &[(String, String)], timeout: Option<Duration>) -> Result<ExecOutput, String> {
        let workdir = if subdir.is_empty() { self.inner_root.clone() } else { format!("{}/{subdir}", self.inner_root) };
        let base = self.base_env();
        let env: Vec<(String, String)> = base.into_iter().chain(env.iter().cloned()).collect();
        let env = &env[..];
        let mut cmd = match self.kind {
            Kind::Lima => {
                let mut c = Command::new("limactl");
                c.args(["shell", "--workdir", &workdir, &self.name]);
                // Environment goes in the script: ssh does not carry it.
                let mut prefix = String::new();
                for (k, v) in env {
                    // `$HOME`-relative values expand; anything else is quoted.
                    if let Some(rest) = v.strip_prefix("$HOME/") {
                        prefix.push_str(&format!("export {k}=\"$HOME\"/{}; ", shell_quote(rest)));
                    } else {
                        prefix.push_str(&format!("export {k}={}; ", shell_quote(v)));
                    }
                }
                c.args(["sh", "-lc", &format!("{prefix}{script}")]);
                c
            }
            Kind::Docker | Kind::AppleContainer => {
                let mut c = Command::new(if self.kind == Kind::Docker { "docker" } else { "container" });
                c.args(["exec", "-w", &workdir]);
                let mut prefix = String::new();
                for (k, v) in env {
                    if let Some(rest) = v.strip_prefix("$HOME/") {
                        prefix.push_str(&format!("export {k}=\"$HOME\"/{}; ", shell_quote(rest)));
                    } else {
                        c.arg("-e").arg(format!("{k}={v}"));
                    }
                }
                c.arg(&self.name).args(["sh", "-lc", &format!("{prefix}{script}")]);
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

    /// A command that runs `program args…` inside the sandbox, in `subdir`
    /// of the worktree, with `env` — stdin and stdout left for the caller
    /// to pipe. For a long-lived process such as an MCP server, where
    /// [`Sandbox::exec`]'s run-to-completion does not fit.
    pub fn command(&self, program: &str, args: &[String], env: &std::collections::BTreeMap<String, String>, subdir: &str) -> Command {
        let workdir = if subdir.is_empty() { self.inner_root.clone() } else { format!("{}/{subdir}", self.inner_root) };
        match self.kind {
            Kind::Lima => {
                let mut c = Command::new("limactl");
                c.args(["shell", "--workdir", &workdir, &self.name]);
                let mut script = String::new();
                for (k, v) in env {
                    script.push_str(&format!("export {k}={}; ", shell_quote(v)));
                }
                script.push_str("exec ");
                script.push_str(&shell_quote(program));
                for a in args {
                    script.push(' ');
                    script.push_str(&shell_quote(a));
                }
                c.args(["sh", "-lc", &script]);
                c
            }
            Kind::Docker | Kind::AppleContainer => {
                let mut c = Command::new(if self.kind == Kind::Docker { "docker" } else { "container" });
                c.args(["exec", "-i", "-w", &workdir]);
                for (k, v) in env {
                    c.arg("-e").arg(format!("{k}={v}"));
                }
                // Through a login shell, so what provisioning put on PATH is found.
                let mut script = String::from("exec ");
                script.push_str(&shell_quote(program));
                for a in args {
                    script.push(' ');
                    script.push_str(&shell_quote(a));
                }
                c.arg(&self.name).args(["sh", "-lc", &script]);
                c
            }
        }
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

/// How to install one toolchain in a Debian-family sandbox, as root or
/// through passwordless sudo (a Lima VM's user has it).
struct Recipe {
    name: &'static str,
    script: &'static str,
    /// A shell test that says the recipe's result is there, for programs
    /// that are not commands on the PATH (an npm package, say). `None`
    /// means `command -v <program>`.
    check: Option<&'static str>,
}

/// `apt-get` with the right prefix, as a shell fragment the recipes share.
const APT: &str = r#"
set -e
if [ "$(id -u)" = 0 ]; then SUDO=""; else SUDO="sudo -n"; fi
export DEBIAN_FRONTEND=noninteractive
apt_install() { $SUDO apt-get update -qq >/dev/null 2>&1 || true; $SUDO apt-get install -y -qq "$@" >/dev/null; }
add_path() { grep -qs "$1" "$HOME/.profile" 2>/dev/null || printf 'export PATH="%s:$PATH"
' "$1" >> "$HOME/.profile"; export PATH="$1:$PATH"; }
"#;

/// The recipe that provides `program`, if there is one.
fn recipe_for(program: &str) -> Option<Recipe> {
    let mut check: Option<&'static str> = None;
    let (name, body): (&'static str, &'static str) = match program {
        "flutter" | "dart" => ("Flutter", r#"
apt_install git curl unzip xz-utils zip libglu1-mesa ca-certificates
if [ ! -d "$HOME/flutter" ]; then git clone --quiet --depth 1 -b stable https://github.com/flutter/flutter.git "$HOME/flutter"; fi
add_path "$HOME/flutter/bin"
add_path "$HOME/.pub-cache/bin"
git config --global --add safe.directory "$HOME/flutter" || true
flutter --version >/dev/null
"#),
        "cargo" | "rustc" | "rustfmt" => ("Rust", r#"
apt_install curl build-essential pkg-config libssl-dev ca-certificates
if [ ! -x "$HOME/.cargo/bin/cargo" ]; then curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null; fi
add_path "$HOME/.cargo/bin"
cargo --version >/dev/null
"#),
        "node" | "npm" | "npx" | "pnpm" | "yarn" => ("Node.js", r#"
apt_install nodejs npm ca-certificates
$SUDO npm install -g --silent corepack >/dev/null 2>&1 || true
corepack enable >/dev/null 2>&1 || $SUDO corepack enable >/dev/null 2>&1 || true
node --version >/dev/null
"#),
        "python" | "python3" | "pip" | "pytest" | "uv" | "ruff" => ("Python", r#"
apt_install python3 python3-pip python3-venv
python3 -m pip install --user --quiet --break-system-packages pytest uv ruff >/dev/null 2>&1 || python3 -m pip install --user --quiet pytest uv ruff >/dev/null
add_path "$HOME/.local/bin"
python3 --version >/dev/null
"#),
        "go" | "gofmt" => ("Go", r#"
apt_install golang-go
go version >/dev/null
"#),
        "mix" | "elixir" => ("Elixir", r#"
apt_install elixir
mix --version >/dev/null
"#),
        "bundle" | "ruby" | "rspec" | "rake" => ("Ruby", r#"
apt_install ruby-full build-essential
gem install --user-install --no-document bundler rspec >/dev/null 2>&1 || true
add_path "$(ruby -e 'puts Gem.user_dir')/bin"
ruby --version >/dev/null
"#),
        "claude" => ("Claude Code", r#"
apt_install curl ca-certificates git
if ! command -v claude >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/claude" ]; then curl -fsSL https://claude.ai/install.sh | bash >/dev/null 2>&1; fi
add_path "$HOME/.local/bin"
claude --version >/dev/null
"#),
        "playwright" => {
            check = Some(r#"test -x "$HOME/.devdock-playwright/node_modules/.bin/playwright" && ls "$HOME/.cache/ms-playwright" 2>/dev/null | grep -q chromium"#);
            ("a headless browser (Playwright Chromium)", r#"
apt_install nodejs npm ca-certificates python3
mkdir -p "$HOME/.devdock-playwright" && cd "$HOME/.devdock-playwright"
[ -f package.json ] || npm init -y >/dev/null 2>&1
[ -x node_modules/.bin/playwright ] || npm install --no-audit --no-fund playwright >/dev/null
npx playwright install --with-deps chromium >/dev/null
npx playwright --version >/dev/null
"#)
        }
        "xvfb-run" => ("a virtual display (Xvfb with Mesa)", r#"
apt_install xvfb libgl1 libegl1 libgl1-mesa-dri libxkbcommon0 libxkbcommon-x11-0 libxi6 libxcursor1 libxrandr2 libxinerama1 libx11-xcb1 libxcb-render0 libxcb-shape0 libxcb-xfixes0 libwayland-client0 libfontconfig1 fonts-dejavu-core
xvfb-run --help >/dev/null 2>&1 || true
"#),
        "opencode" => ("OpenCode", r#"
apt_install curl ca-certificates git unzip
if [ ! -x "$HOME/.opencode/bin/opencode" ]; then curl -fsSL https://opencode.ai/install | bash >/dev/null 2>&1; fi
add_path "$HOME/.opencode/bin"
opencode --version >/dev/null
"#),
        "make" => ("build tools", r#"
apt_install build-essential
"#),
        "gradle" | "./gradlew" | "mvn" | "./mvnw" | "java" => ("Java", r#"
apt_install default-jdk gradle maven
java -version >/dev/null 2>&1
"#),
        _ => return None,
    };
    // Leaked once per distinct recipe: a handful of static strings.
    let script: &'static str = Box::leak(format!("{APT}
{body}").into_boxed_str());
    Some(Recipe { name, script, check })
}

/// Claude Code's own credentials on this machine, as the JSON its Linux
/// build reads from `~/.claude/.credentials.json`: from the file when
/// there is one, else from the macOS Keychain item it keeps them in.
fn host_claude_credentials() -> Option<String> {
    if let Some(home) = dirs::home_dir() {
        if let Ok(text) = std::fs::read_to_string(home.join(".claude/.credentials.json")) {
            if text.contains("claudeAiOauth") || text.contains("apiKey") {
                return Some(text);
            }
        }
    }
    if cfg!(target_os = "macos") {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
                return Some(text);
            }
        }
    }
    None
}

/// `program args…` as one shell line, each word quoted.
pub fn shell_words(words: &[String]) -> String {
    words.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The sandbox as a place to start an MCP server: the server's process
/// runs inside, over the worktree at its inner path.
impl crate::agent::mcp::Launcher for SandboxRunner {
    fn command(&self, program: &str, args: &[String], env: &std::collections::BTreeMap<String, String>, workdir: &Path) -> Command {
        let subdir = workdir.strip_prefix(self.0.root()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        self.0.command(program, args, env, &subdir)
    }
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

    #[test]
    fn every_toolchain_has_a_recipe_that_sh_accepts() {
        for program in ["flutter", "dart", "cargo", "npm", "python3", "pytest", "go", "mix", "bundle", "make", "gradle", "claude", "playwright", "xvfb-run", "opencode"] {
            let recipe = recipe_for(program).unwrap_or_else(|| panic!("no recipe for {program}"));
            assert!(recipe.script.contains("set -e"));
            // `sh -n` parses without running.
            let out = Command::new("sh").arg("-n").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().and_then(|mut child| {
                use std::io::Write as _;
                child.stdin.take().unwrap().write_all(recipe.script.as_bytes())?;
                child.wait_with_output()
            }).unwrap();
            assert!(out.status.success(), "{program}: {}", String::from_utf8_lossy(&out.stderr));
        }
        assert!(recipe_for("frobnicate").is_none());
        assert_eq!(recipe_for("dart").unwrap().name, "Flutter");
    }

    /// The Claude Code CLI provisioned inside the sandbox and runnable
    /// from a login shell there. Credentials are the developer's to seed.
    /// `cargo test --lib sandbox::tests::live_claude -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_claude_code_is_provisioned_in_the_sandbox() {
        if installed().is_empty() {
            eprintln!("no runtime installed; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::start(&Spec::default(), tmp.path(), &mut |l| println!("  {l}")).unwrap();
        sandbox.provision(&["claude"], &mut |l| println!("  {l}")).unwrap();
        let out = sandbox.exec("claude --version", "", &[], Some(Duration::from_secs(60))).unwrap();
        assert!(out.success, "{}{}", out.stdout, out.stderr);
        assert!(out.stdout.contains("Claude Code"), "{}", out.stdout);
        // The launch command DevDock would use, without running a task.
        let mut cmd = sandbox.command("claude", &["--version".into()], &Default::default(), "");
        let out = cmd.stdin(Stdio::null()).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("Claude Code"));
        println!("claude inside: {}", out.stdout.iter().map(|b| *b as char).collect::<String>().trim());
    }

    /// This repository's own `[[screenshot]]` entries rendered in the
    /// sandbox: Rust provisioned, the app built for Linux under Xvfb, three
    /// PNGs out. Slow the first time (a full build in the VM).
    /// `cargo test --lib sandbox::tests::live_devdock -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_devdock_photographs_itself_in_the_sandbox() {
        if installed().is_empty() {
            eprintln!("no runtime installed; skipping");
            return;
        }
        let root = std::env::current_dir().unwrap();
        let mut log = |l: String| println!("  {l}");
        let sandbox = std::sync::Arc::new(Sandbox::start(&Spec::default(), &root, &mut log).unwrap());
        sandbox.provision(&["cargo", "xvfb-run"], &mut log).unwrap();
        let mut runners = crate::local_ci::runner::RunnerRegistry::with_builtins();
        runners.register(Box::new(SandboxRunner(sandbox.clone())));
        let shots = crate::screenshots::capture(&root, &runners, Some(&sandbox), "live-devdock", &mut log).shots;
        let names: Vec<String> = shots.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        println!("{names:?}");
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(!root.join(".devdock").exists());
    }

    /// Flutter, the toolchain a plain image is least likely to have,
    /// installed into the sandbox and found by a login shell — the case of
    /// a Flutter repository's `dart analyze` check. Slow the first time.
    /// `cargo test --lib sandbox::tests::live_flutter -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_flutter_is_provisioned_and_dart_analyze_runs() {
        if installed().is_empty() {
            eprintln!("no runtime installed; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pubspec.yaml"), "name: rules\nenvironment:\n  sdk: ^3.0.0\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::write(tmp.path().join("lib/rules.dart"), "int twice(int x) => x * 2;\n").unwrap();
        let sandbox = Sandbox::start(&Spec::default(), tmp.path(), &mut |l| println!("  {l}")).unwrap();
        sandbox.provision(&["dart"], &mut |l| println!("  {l}")).unwrap();
        let out = sandbox.exec("dart --version && dart pub get && dart analyze", "", &[], Some(Duration::from_secs(600))).unwrap();
        println!("{}{}", out.stdout, out.stderr);
        assert!(out.success, "{}{}", out.stdout, out.stderr);
        assert!(out.stdout.contains("No issues found") || out.stderr.contains("No issues found"), "{}{}", out.stdout, out.stderr);
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
        // Provisioning: a toolchain the sandbox lacks is installed and then
        // found by a login shell, as a check would find it.
        sandbox.provision(&["python3", "pytest"], &mut |l| println!("  {l}")).unwrap();
        let out = sandbox.exec("python3 -c 'print(1+1)' && pytest --version", "", &[], Some(Duration::from_secs(120))).unwrap();
        assert!(out.success, "{}{}", out.stdout, out.stderr);
        assert!(out.stdout.contains('2'));
        println!("describe: {}", sandbox.describe());
    }
}
