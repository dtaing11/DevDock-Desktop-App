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
    /// Directory of the config file this job came from, relative to the
    /// repository root; empty for the root config. The job's commands run
    /// here, so a monorepo package's `cargo test` runs in that package.
    ///
    /// Filled in by [`discover_configs`], never read from the TOML — a config
    /// does not get to claim it lives somewhere else.
    #[serde(skip)]
    pub dir: String,
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

    /// Name for the UI, qualified by directory when the job came from a
    /// nested config. Two packages can both call a job "tests"; without the
    /// prefix the results list would be ambiguous.
    pub fn display_name(&self) -> String {
        if self.dir.is_empty() {
            self.name.clone()
        } else {
            format!("{}: {}", self.dir, self.name)
        }
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
    /// AI code review gate, run after the jobs pass. See [`crate::review`].
    #[serde(default)]
    pub review: crate::review::ReviewConfig,
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

/// Cap on how many config files are loaded, so a pathological tree cannot
/// spawn hundreds of jobs.
pub const MAX_CONFIGS: usize = 25;

/// Every config in the repository, merged: the root one plus any in
/// subdirectories.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfigs {
    /// Jobs from every config, each tagged with its directory, plus the
    /// **root** config's gate settings.
    pub config: Config,
    /// Directories that contributed jobs, in load order. `""` is the root.
    pub sources: Vec<String>,
    /// Directories whose `[on_push]` / `[review]` sections were ignored
    /// because those gates are repository-wide. Surfaced to the user rather
    /// than dropped silently.
    pub ignored_gates: Vec<String>,
}

/// Finds every `.git-manage-ci.toml` in the repository and merges them.
///
/// Discovery goes through `git ls-files --cached --others --exclude-standard`,
/// which means `.gitignore` is honoured for free: configs under `target/`,
/// `node_modules/`, or any ignored path are never picked up, and a brand-new
/// uncommitted config still is. Falls back to the root config alone when the
/// path is not a git repository.
///
/// Jobs from a nested config run **in that config's directory**, so a
/// monorepo package's `cargo test` runs in the package rather than the root.
///
/// `[on_push]` and `[review]` are taken from the root config only. A push
/// publishes the whole repository, so a per-directory push gate has no
/// coherent meaning; nested ones are reported in
/// [`LoadedConfigs::ignored_gates`].
pub fn discover_configs(repo_root: &Path) -> Result<LoadedConfigs> {
    let mut loaded = LoadedConfigs::default();

    // Root config first, so its gates win and its jobs list first.
    if let Some(root) = load_config(repo_root)? {
        loaded.config.on_push = root.on_push;
        loaded.config.review = root.review;
        loaded.sources.push(String::new());
        loaded.config.jobs.extend(tag_jobs(root.jobs, ""));
    }

    for dir in nested_config_dirs(repo_root) {
        if loaded.sources.len() >= MAX_CONFIGS {
            break;
        }
        // A broken nested config must not take down the whole run; skip it
        // and keep the configs that do parse.
        let Ok(Some(nested)) = load_config(&repo_root.join(&dir)) else { continue };
        if nested.on_push.run || nested.review.runs_at_all() {
            loaded.ignored_gates.push(dir.clone());
        }
        if nested.jobs.is_empty() {
            continue;
        }
        loaded.config.jobs.extend(tag_jobs(nested.jobs, &dir));
        loaded.sources.push(dir);
    }

    Ok(loaded)
}

fn tag_jobs(jobs: Vec<Job>, dir: &str) -> Vec<Job> {
    jobs.into_iter().map(|mut j| { j.dir = dir.to_string(); j }).collect()
}

/// Directories (relative, never empty) holding a non-root config file,
/// sorted so the job order is stable between runs.
fn nested_config_dirs(repo_root: &Path) -> Vec<String> {
    let Ok(repo) = crate::git::Repo::open(repo_root) else { return Vec::new() };
    // `--others --exclude-standard` adds untracked-but-not-ignored files, so a
    // config added and not yet committed is still found.
    let Ok(out) = repo.git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    else {
        return Vec::new();
    };

    let mut dirs: Vec<String> = out
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter_map(|path| {
            let rest = path.strip_suffix(CONFIG_FILE)?;
            // The root config is loaded separately.
            let dir = rest.strip_suffix('/')?;
            (!dir.is_empty()).then(|| dir.to_string())
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
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

# Have an AI review the outgoing diff before a push or pull request. It runs
# after the jobs above pass, reports findings with its reasoning, and you can
# always choose to proceed anyway.
# [review]
# run = true                # review before both pushes and pull requests
# on_push = true            # or gate each trigger on its own — either one
# on_pull_request = true    # falls back to `run` when left out
# block_on_failure = true   # findings at or above fail_on stop to ask first
# fail_on = "high"          # low | medium | high
# provider = "claude"       # claude | ollama; defaults to the app's selection
# model = "claude-opus-5"
# max_diff_bytes = 24000
# instructions = "Flag any new blocking call on the UI thread."

# Want the review in your own format instead of the built-in findings list?
# Set output = "markdown" and describe the shape you want. It is rendered as
# Markdown in the app. With block_on_failure = true the model also leads with
# a "VERDICT: block" or "VERDICT: pass" line, which drives the gate and is not
# shown.
# [review]
# run = true
# output = "markdown"
# output_instructions = """
# ## Verdict
# One sentence.
# ## Must fix
# Bullets, each with `file:line` and the failing case.
# ## Nits
# Bullets, or "none".
# """
"#;
    std::fs::write(repo_root.join(CONFIG_FILE), template).map_err(|e| CiError(e.to_string()))
}

/// The system prompt for AI config generation, distilled from
/// docs/local-ci.md so the model writes valid `.git-manage-ci.toml`.
pub const AI_CONFIG_SYSTEM_PROMPT: &str = r#"You are an expert build engineer writing a local CI config file named .git-manage-ci.toml for the DevDock git client. Output ONLY valid TOML, no markdown fences, no explanation.

Format specification:
- Each [[job]] block is one check. Jobs run in parallel. Fields:
  - name (string, required): shown in the UI.
  - commands (array of strings, required): shell commands run in order inside the repo root; the job stops at the first failing command.
  - image (string, optional): a Docker image; when set, commands run inside that container with the repo mounted at /work. Omit to run directly on the host machine.
  - env (inline table, optional): extra environment variables, e.g. env = { RUST_BACKTRACE = "1" }.
  - secrets (array of strings, optional): names of secrets whose values come from the untracked .git-manage-ci.secrets file.
- An optional [on_push] section controls push integration:
  [on_push]
  run = true               # run all jobs automatically before every push
  block_on_failure = true  # failing jobs cancel the push
- An optional [review] section adds an AI review of the outgoing diff, run
  after the jobs pass:
  [review]
  run = true               # both triggers; or set on_push / on_pull_request
  block_on_failure = true  # findings at or above fail_on stop to ask first
  fail_on = "high"         # low | medium | high
  # instructions = "..."   # project-specific things to look for
  # output = "markdown"    # answer in the project's own format instead of
  # output_instructions = "..."   # findings; describe the shape you want

Identify the project before writing anything. The user message gives you, in order: a count of files by extension, the full list of tracked files, and the contents of the project's configuration files. Work out from that evidence what the project is and which toolchain builds it. The extension counts tell you what the code is written in; the config file contents tell you how it is built and tested.

Then write the config as a TOML comment on the first line stating what you concluded, e.g. `# Stack: Flutter (Dart)` — the developer reviews this before saving, so a wrong guess is visible immediately.

Guidelines:
- Every command must be justified by something in the scan. Do not propose a command for a toolchain you cannot see evidence of: a package manager with no manifest, a test runner not in the dependencies. If the evidence does not tell you what to run, emit a job with a `# TODO` comment naming what you would need instead of inventing a command.
- Prefer commands the project already defines over ones you know generically: a Makefile or justfile target, a script in the manifest, or a step from a CI workflow included in the scan. Those are known to work in this repository.
- Typical jobs: lint/format check, tests, build.
- Prefer fast, deterministic commands that exist in the project (e.g. use the project's own scripts when present).
- Only suggest a Docker image when the project clearly benefits (e.g. pinned toolchain); otherwise run on the host.
- Do not invent commands for tools the project does not use.
- Include [on_push] with run = true and block_on_failure = true unless the project seems experimental.
- Include [review] with run = true, block_on_failure = true, and fail_on = "high". Add `instructions` only when the repository has a genuine project-specific rule worth stating; leave it out otherwise.
- Add short `#` comments explaining non-obvious choices."#;

/// Collects a compact description of the repository for the AI: top-level
/// file listing plus the contents of common build/manifest files. Bounded
/// so it fits in a prompt.
/// Total scan budget, and the per-file cap when inlining contents.
const SCAN_BUDGET: usize = 24_000;
const FILE_SNIPPET: usize = 2_000;

/// Names and suffixes never worth inlining: generated, huge, or no signal
/// about how the project is built. Deliberately tiny — this is about token
/// budget, not about knowing ecosystems.
const NOT_WORTH_INLINING: &[&str] = &[
    ".lock", "-lock.json", ".sum", ".min.js", ".map", ".svg", ".png", ".jpg", ".jpeg",
    ".gif", ".ico", ".ttf", ".otf", ".woff", ".woff2", ".pdf", ".zip", ".gz", ".icns",
    "LICENSE", "LICENCE", "COPYING", ".gitignore", ".gitattributes",
];

/// Collects a description of the repository for the AI: a file-type census,
/// the full tracked listing, and the contents of the shallow config files.
///
/// Everything here is **evidence, not conclusions**. There is deliberately no
/// table mapping marker files to stack names: such a list needs updating for
/// every framework, is wrong for anything not in it, and replaces a judgement
/// the model makes better. The original bug was not that the model could not
/// recognise Flutter — it was that this scan showed only the top directory and
/// never inlined `pubspec.yaml`, so there was nothing to recognise.
pub fn repo_scan(repo_root: &Path) -> String {
    let files = tracked_files(repo_root);
    let census = extension_census(&files);
    let source = source_extensions(&census, files.len());

    // The config file contents are the highest-signal part of the scan, so
    // they are built *first* and get the budget. Doing this after the file
    // listing meant a large repo spent the whole budget on paths and never
    // reached its manifest — which reproduced the original bug (a Flutter
    // repo with 1868 files whose pubspec.yaml never made it into the prompt).
    let mut manifests = String::new();
    for path in inlinable_files(&files, &source) {
        if manifests.len() > SCAN_BUDGET * 2 / 3 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(repo_root.join(&path)) else { continue };
        if text.trim().is_empty() {
            continue;
        }
        let mut snippet: String = text.chars().take(FILE_SNIPPET).collect();
        if snippet.len() < text.len() {
            snippet.push_str("\n[truncated]");
        }
        manifests.push_str(&format!("\n--- {path} ---\n{snippet}\n"));
    }

    let mut out = String::new();

    // 1. Extension census: the clearest single signal of what the code is,
    //    and pure counting — no per-language knowledge involved.
    if !census.is_empty() {
        out.push_str("Code files by type:\n");
        for (ext, count) in census.iter().take(20) {
            out.push_str(&format!("  {count:>5}  .{ext}\n"));
        }
        out.push('\n');
    }

    // 2. Configuration contents.
    out.push_str(&manifests);

    // 3. The listing last, with whatever budget is left: it is context, and
    //    the most truncatable part.
    out.push_str(&format!("\nTracked files ({} total):\n", files.len()));
    let room = SCAN_BUDGET.saturating_sub(out.len());
    let mut shown = 0;
    let mut used = 0;
    for path in &files {
        if used + path.len() + 3 > room {
            break;
        }
        used += path.len() + 3;
        shown += 1;
        out.push_str("  ");
        out.push_str(path);
        out.push('\n');
    }
    if shown < files.len() {
        out.push_str(&format!("  … and {} more\n", files.len() - shown));
    }
    out
}

/// Paths worth inlining, shallowest first: manifests, task runners, CI
/// workflows, and the README all fall out of this without being named.
fn inlinable_files(files: &[String], source_exts: &[String]) -> Vec<String> {
    let depth = |p: &str| p.matches('/').count();
    let is_source = |p: &str| {
        p.rsplit_once('.').map(|(_, e)| source_exts.iter().any(|s| s == e)).unwrap_or(false)
    };
    let is_doc = |p: &str| {
        let lower = p.to_ascii_lowercase();
        lower.ends_with(".md") || lower.ends_with(".rst") || lower.ends_with(".txt")
    };
    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|p| {
            // Depth 0-2 covers root manifests plus things like
            // .github/workflows/ci.yml and packages/api/pubspec.yaml.
            depth(p) <= 2
                && !NOT_WORTH_INLINING.iter().any(|s| p.ends_with(s))
                // The project's own source is described by the census; its
                // contents would crowd out the configuration.
                && !is_source(p)
        })
        .collect();
    // Configuration before prose. A repo with several long markdown documents
    // would otherwise spend the budget on them and push its actual manifest
    // out — which is how a Flutter monorepo lost `frontend/pubspec.yaml` to
    // its README. Docs are still useful context, so a couple are kept.
    candidates.sort_by_key(|p| (is_doc(p), depth(p), p.len(), (*p).clone()));
    let mut docs = 0;
    candidates
        .into_iter()
        .filter(|p| {
            if !is_doc(p) {
                return true;
            }
            docs += 1;
            docs <= 2
        })
        .take(30)
        .cloned()
        .collect()
}

/// Extensions that make up a large share of the repository — the project's
/// own source. Derived from the census, so it needs no per-language table:
/// whatever this project is mostly written in counts as source, whether that
/// is `.dart`, `.zig`, or something nobody has heard of.
///
/// The share threshold is what keeps a README inlinable: `.md` at 1% of files
/// is documentation, `.dart` at 75% is the codebase.
fn source_extensions(census: &[(String, usize)], total: usize) -> Vec<String> {
    let floor = (total / 10).max(5);
    census
        .iter()
        .filter(|(_, count)| *count >= floor)
        .map(|(ext, _)| ext.clone())
        .collect()
}

/// Repository-relative paths of tracked and not-ignored files, via git so
/// `.gitignore` is honoured. Falls back to a top-level listing outside a repo.
fn tracked_files(repo_root: &Path) -> Vec<String> {
    if let Ok(repo) = crate::git::Repo::open(repo_root) {
        if let Ok(out) =
            repo.git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        {
            let mut files: Vec<String> =
                out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect();
            files.sort();
            return files;
        }
    }
    let Ok(entries) = std::fs::read_dir(repo_root) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != ".git")
        .collect();
    names.sort();
    names
}

/// Binary and asset extensions excluded from the census. A Flutter app with
/// 35 icons and 20 Dart files is a Dart project; counting the icons first
/// buries the signal. This is about file encoding, not about ecosystems, so
/// unlike a stack taxonomy it does not need per-framework upkeep.
const ASSET_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "icns", "svg", "pdf", "ttf", "otf",
    "woff", "woff2", "eot", "mp3", "mp4", "wav", "mov", "avi", "zip", "gz", "tar", "bz2",
    "7z", "rar", "jar", "so", "dylib", "dll", "a", "o", "class", "pyc", "wasm", "bin",
    "exe", "lock", "riv", "psd", "ai", "sketch", "keystore", "jks",
];

/// Extension counts of *code and configuration*, most common first.
/// Extensionless files and assets are skipped.
fn extension_census(files: &[String]) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for path in files {
        let name = path.rsplit('/').next().unwrap_or(path);
        if let Some((_stem, ext)) = name.rsplit_once('.') {
            let asset = ASSET_EXTS.iter().any(|a| a.eq_ignore_ascii_case(ext));
            if !ext.is_empty() && ext.len() <= 12 && !name.starts_with('.') && !asset {
                *counts.entry(ext).or_default() += 1;
            }
        }
    }
    let mut census: Vec<(String, usize)> =
        counts.into_iter().map(|(e, c)| (e.to_string(), c)).collect();
    // Count descending, then name, so the order is stable.
    census.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    census
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
        name: job.display_name(),
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
    let request = ExecRequest {
        repo_root,
        work_subdir: &job.dir,
        script: &script,
        env: &env,
        target: job.target(),
    };
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
                name: job.display_name(),
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
    // Discovery, not load_config: the CLI and the pre-push hook must see the
    // same jobs as the app, including a monorepo's per-package configs.
    // Running only the root config here would let `devdock ci` (and therefore
    // the hook that gates pushes) pass while the app reports failures.
    let loaded = discover_configs(repo_root)?;
    let config = loaded.config;
    if loaded.sources.is_empty() {
        println!("devdock ci: no {CONFIG_FILE} found, nothing to run");
        return Ok(true);
    }
    if config.jobs.is_empty() {
        println!("devdock ci: no jobs configured");
        return Ok(true);
    }
    for dir in &loaded.ignored_gates {
        println!(
            "devdock ci: note: [on_push]/[review] in {dir} ignored (they are \
             repository-wide; set them in the root {CONFIG_FILE})"
        );
    }
    let mut all_ok = true;
    for job in &config.jobs {
        print!("devdock ci: {} ... ", job.display_name());
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
            dir: String::new(),
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
            dir: String::new(),
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
            dir: String::new(),
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
            dir: String::new(),
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
            dir: String::new(),
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

#[cfg(test)]
mod ai_config_tests {
    use super::*;

    #[test]
    fn repo_scan_lists_files_and_manifest_contents() {
        let tmp = std::env::temp_dir().join(format!("devdock-scan-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        let scan = repo_scan(&tmp);
        // Outside a git repo the listing falls back to the top level, but the
        // manifest contents must still be inlined.
        assert!(scan.contains("Cargo.toml"), "{scan}");
        assert!(scan.contains("name = \"demo\""), "{scan}");
        assert!(scan.contains("Tracked files"), "{scan}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The census is what settles "what kind of project is this", and it is
    /// pure counting — no per-language table involved.
    #[test]
    fn census_counts_extensions_most_common_first() {
        let files: Vec<String> = ["lib/a.dart", "lib/b.dart", "lib/c.dart", "pubspec.yaml", "x.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let census = extension_census(&files);
        assert_eq!(census[0], ("dart".to_string(), 3), "got {census:?}");
        assert!(census.contains(&("yaml".to_string(), 1)));
    }

    /// Shallow config files get inlined whatever they are called, so an
    /// ecosystem nobody enumerated still has its manifest read. This is the
    /// property that replaced a hard-coded manifest list.
    #[test]
    fn inlining_is_selected_by_shape_not_by_name() {
        let files: Vec<String> = [
            "pubspec.yaml",       // Flutter — never named in this file
            "flake.nix",          // Nix — never named either
            "zig.build",
            "README.md",          // documentation: useful context, inlined
            "Cargo.lock",         // generated: skipped
            "LICENSE",            // no signal: skipped
            "assets/logo.png",    // binary: skipped
            ".github/workflows/ci.yml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let picked = inlinable_files(&files, &[]);
        for want in
            ["pubspec.yaml", "flake.nix", "zig.build", "README.md", ".github/workflows/ci.yml"]
        {
            assert!(picked.iter().any(|p| p == want), "{want} should be inlined: {picked:?}");
        }
        for skip in ["Cargo.lock", "LICENSE", "assets/logo.png"] {
            assert!(!picked.iter().any(|p| p == skip), "{skip} should be skipped: {picked:?}");
        }
    }

    /// The project's own source is excluded from inlining, and which
    /// extensions count as source comes from the census rather than a table —
    /// so this works for a language nobody enumerated.
    #[test]
    fn dominant_extensions_are_treated_as_source() {
        // 30 .dart files, one README: .dart is the codebase, .md is not.
        let mut files: Vec<String> = (0..30).map(|i| format!("lib/f{i}.dart")).collect();
        files.push("README.md".to_string());
        files.push("pubspec.yaml".to_string());

        let census = extension_census(&files);
        let source = source_extensions(&census, files.len());
        assert!(source.iter().any(|e| e == "dart"), "dart should be source: {source:?}");
        assert!(!source.iter().any(|e| e == "md"), "a lone README is not source: {source:?}");

        let picked = inlinable_files(&files, &source);
        assert!(picked.iter().any(|p| p == "pubspec.yaml"));
        assert!(picked.iter().any(|p| p == "README.md"));
        assert!(!picked.iter().any(|p| p.ends_with(".dart")), "source leaked in: {picked:?}");
    }

    /// Assets must not outrank code in the census. A Flutter app with more
    /// icons than Dart files is still a Dart project.
    #[test]
    fn census_ignores_assets() {
        let mut files: Vec<String> = (0..40).map(|i| format!("assets/i{i}.png")).collect();
        files.extend((0..8).map(|i| format!("lib/f{i}.dart")));
        let census = extension_census(&files);
        assert_eq!(census[0], ("dart".to_string(), 8), "assets buried the code: {census:?}");
        assert!(!census.iter().any(|(e, _)| e == "png"));
    }

    /// Regression: in a large repository the file listing used to consume the
    /// whole budget, so the manifest never reached the prompt — the same
    /// failure as the original bug, just triggered by size instead of by a
    /// missing name.
    #[test]
    fn a_large_repository_still_gets_its_manifest_inlined() {
        let tmp = std::env::temp_dir().join(format!("devdock-big-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("lib")).unwrap();
        std::fs::write(
            tmp.join("pubspec.yaml"),
            "name: big_app\ndependencies:\n  flutter:\n    sdk: flutter\n",
        )
        .unwrap();
        // Enough paths that the listing alone would blow the budget.
        for i in 0..3000 {
            std::fs::write(tmp.join("lib").join(format!("a_very_long_file_name_{i}.dart")), "//")
                .ok();
        }

        let scan = repo_scan(&tmp);
        assert!(
            scan.contains("--- pubspec.yaml ---"),
            "the manifest must survive the budget in a big repo"
        );
        assert!(scan.contains("sdk: flutter"), "and its contents, not just its name");
        assert!(scan.len() <= SCAN_BUDGET + FILE_SNIPPET, "scan is {} bytes", scan.len());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn ai_prompt_documents_the_format() {
        // The system prompt must teach the exact schema the parser accepts.
        for needle in ["[[job]]", "commands", "image", "secrets", "[on_push]", "block_on_failure"] {
            assert!(AI_CONFIG_SYSTEM_PROMPT.contains(needle), "missing {needle}");
        }
        // And what the model writes must parse: check the documented example shape.
        let sample = "[[job]]\nname = \"tests\"\ncommands = [\"cargo test\"]\n\n[on_push]\nrun = true\nblock_on_failure = true\n";
        let config: Config = toml::from_str(sample).unwrap();
        assert_eq!(config.jobs.len(), 1);
        assert!(config.on_push.run && config.on_push.block_on_failure);
    }
}
