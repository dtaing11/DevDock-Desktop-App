//! Stacked pull requests, driven by GitHub's own `gh stack` extension.
//!
//! A stack is how one large change gets reviewed as several small ones. Rather
//! than a single branch off `main` carrying twelve commits, each self-contained
//! piece gets its own branch and its own pull request, and each PR targets the
//! branch below it instead of the trunk. Reviewers see one focused diff per PR.
//!
//! GitHub now has a first-class stack object and a CLI extension that manages
//! it — [`github/gh-stack`](https://github.com/github/gh-stack). This module is
//! a thin, typed wrapper over that extension rather than a second
//! implementation of the same idea:
//!
//! - the chain itself lives in `.git/gh-stack`, written and read by `gh`;
//! - [`stack_for`] reads it through `gh stack view --json` and decorates each
//!   branch with its commits, so the view can say what a branch carries;
//! - every operation — [`branch_on_tip`], [`restack`], [`push_stack`],
//!   [`submit`], [`sync`] — is one `gh stack` command run non-interactively,
//!   with the app's GitHub token handed over so nobody has to `gh auth login`
//!   separately.
//!
//! The one thing `gh stack` only does interactively is restructuring a stack
//! (`gh stack modify`, a TUI). The app opens that in its terminal panel rather
//! than reimplementing it against the extension's private state file.
//!
//! If `gh` or the extension is missing, every operation fails with the install
//! command in the message; see [`INSTALL_HINT`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::git::{Commit, FileStatus, GitError, Repo, Result};

/// How to get the tooling this module needs.
pub const INSTALL_HINT: &str =
    "Install the GitHub CLI, then run: gh extension install github/gh-stack";

/// How many commits of one entry are loaded for display.
const MAX_ENTRY_COMMITS: u32 = 100;

/// The file `gh stack rebase` leaves behind while it is stopped on a conflict.
const REBASE_STATE: &str = "gh-stack-rebase-state";

/// The exit status `gh stack rebase` uses for "stopped on a conflict".
const EXIT_CONFLICT: i32 = 3;

/// Config key suffixes the previous, built-in implementation used for the
/// parent link and the pull request number. Read once to migrate; never
/// written.
const LEGACY_PARENT_KEY: &str = "devdock-parent";
const LEGACY_PR_KEY: &str = "devdock-pr";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One branch in a stack, together with the branch it sits on.
#[derive(Debug, Clone, Default)]
pub struct StackEntry {
    pub branch: String,
    /// The branch this one is based on: the base of its pull request. The
    /// nearest branch below that has not merged, or the trunk.
    pub parent: String,
    /// Commits this entry adds on top of its parent (`parent..branch`).
    pub commits: Vec<Commit>,
    /// Number of the pull request for this branch, once one exists.
    pub pr: Option<u64>,
    pub pr_url: Option<String>,
    /// The parent has commits this branch does not: it must be rebased
    /// before its pull request shows the right diff.
    pub needs_restack: bool,
    /// Its pull request merged, or every one of its commits is already in the
    /// trunk by patch id. Either way it has landed.
    pub merged: bool,
    /// Its pull request is in a merge queue.
    pub queued: bool,
}

impl StackEntry {
    /// A one-line description of this entry for the stack view.
    pub fn summary(&self) -> String {
        // `log` is newest-first, so the oldest commit is the one the branch
        // opened with — the closest thing it has to a title.
        self.commits
            .last()
            .map(|c| c.subject.clone())
            .unwrap_or_else(|| "(no commits)".to_string())
    }
}

/// A linear chain of branches, bottom (nearest the trunk) first.
#[derive(Debug, Clone, Default)]
pub struct Stack {
    /// The branch the bottom entry is based on, usually `main`.
    pub trunk: String,
    /// Entries ordered bottom → top. Empty when the branch is not tracked.
    pub entries: Vec<StackEntry>,
    /// Index of the checked-out branch within `entries`, if it is in the stack.
    pub current: Option<usize>,
    /// `gh stack` knows this branch. When false the entries are empty and the
    /// branch can be tracked with [`track`] or [`branch_on_tip`].
    pub tracked: bool,
    /// A restack stopped on a conflict and is waiting to be continued or
    /// aborted.
    pub restacking: bool,
    /// A chain recorded by the previous, built-in implementation (in git
    /// config) that `gh stack` does not know about yet. Bottom first.
    pub legacy: Vec<String>,
}

impl Stack {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The parent a new branch on top of the stack would have.
    pub fn tip(&self) -> String {
        self.entries.last().map(|e| e.branch.clone()).unwrap_or_else(|| self.trunk.clone())
    }

    pub fn entry(&self, branch: &str) -> Option<&StackEntry> {
        self.entries.iter().find(|e| e.branch == branch)
    }

    /// Entries that must be rebased before their PRs are honest.
    pub fn stale(&self) -> Vec<&StackEntry> {
        self.entries.iter().filter(|e| e.needs_restack).collect()
    }
}

// ---------------------------------------------------------------------------
// Running gh
// ---------------------------------------------------------------------------

/// Where the GitHub CLI is.
///
/// `PATH` first. A GUI app started from the Dock does not get the shell's
/// `PATH`, so the places package managers put `gh` are tried after it.
pub fn gh_program() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("gh");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = vec![
        "/opt/homebrew/bin/gh".into(),
        "/usr/local/bin/gh".into(),
        "/home/linuxbrew/.linuxbrew/bin/gh".into(),
        "/usr/bin/gh".into(),
    ];
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/gh"));
        candidates.push(home.join(".nix-profile/bin/gh"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Whether `gh` and the stack extension can be run at all.
pub fn available() -> bool {
    let Some(program) = gh_program() else { return false };
    Command::new(program)
        .args(["stack", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What one `gh stack` invocation produced.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// Progress lines, for the activity log: stdout then stderr, blank lines
    /// and gh's "what's next" advice dropped.
    fn log(&self) -> Vec<String> {
        self.stdout
            .lines()
            .chain(self.stderr.lines())
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .filter(|l| !is_advice(l))
            .map(|l| l.to_string())
            .collect()
    }

    /// The error, from whatever gh printed, with its glyphs stripped.
    fn error(&self) -> String {
        let mut lines: Vec<&str> = self
            .stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty() && !is_advice(l))
            .collect();
        // gh puts the actual failure on a ✗ line; prefer those.
        let failures: Vec<&str> = lines.iter().copied().filter(|l| l.starts_with('✗')).collect();
        if !failures.is_empty() {
            lines = failures;
        }
        let text = lines
            .iter()
            .map(|l| l.trim_start_matches(['✗', '⚠', ' ']).trim())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            format!("gh stack exited with status {}", self.code)
        } else {
            text
        }
    }
}

/// Lines gh prints to tell a person what to type next. Right in a terminal,
/// noise in a log with buttons above it.
fn is_advice(line: &str) -> bool {
    let l = line.trim();
    l.starts_with('•')
        || l.starts_with("What's next")
        || l.starts_with("To push up")
        || l.starts_with("To create PRs")
        || l.starts_with("Run `gh stack")
        || l.starts_with("Or abort")
        || l.starts_with("Resolve conflicts on")
        || l.starts_with("To resolve:")
        || l.starts_with("Checkout an existing stack")
}

/// Runs `gh stack <args>` in the repository, non-interactively.
///
/// `auth` is the app's GitHub token. It goes to gh as `GH_TOKEN`, and to the
/// git commands gh spawns as an HTTP header via `GIT_CONFIG_*` — the same
/// mechanism [`Repo`] uses, so nothing is written to disk or shown in `ps`.
fn run(repo: &Repo, args: &[&str], auth: Option<&str>) -> Result<Run> {
    let Some(program) = gh_program() else {
        return Err(GitError::Command(format!("The GitHub CLI (gh) was not found. {INSTALL_HINT}")));
    };
    let mut cmd = Command::new(&program);
    cmd.arg("stack")
        .args(args)
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = auth {
        cmd.env("GH_TOKEN", token);
        if origin_is_github(repo) {
            use base64::Engine as _;
            let basic = base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{token}"));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env("GIT_CONFIG_VALUE_0", format!("AUTHORIZATION: basic {basic}"));
        }
    }
    let out = cmd.output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            GitError::Command(format!("Could not run {}. {INSTALL_HINT}", program.display()))
        }
        _ => GitError::Io(e),
    })?;
    let run = Run {
        code: out.status.code().unwrap_or(-1),
        stdout: strip_ansi(&String::from_utf8_lossy(&out.stdout)),
        stderr: strip_ansi(&String::from_utf8_lossy(&out.stderr)),
    };
    if run.stderr.contains("unknown command \"stack\"") {
        return Err(GitError::Command(format!(
            "The gh-stack extension is not installed. {INSTALL_HINT}"
        )));
    }
    Ok(run)
}

/// Runs a command that is expected to succeed, turning any other exit into
/// an error carrying what gh said.
fn run_ok(repo: &Repo, args: &[&str], auth: Option<&str>) -> Result<Run> {
    let run = run(repo, args, auth)?;
    if run.code != 0 {
        return Err(GitError::Command(run.error()));
    }
    Ok(run)
}

/// gh honours `NO_COLOR`, but a stray escape in a log line is worse than a
/// few lines of belt and braces.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn origin_is_github(repo: &Repo) -> bool {
    repo.git(&["remote", "get-url", "origin"])
        .map(|url| url.trim().starts_with("https://github.com/"))
        .unwrap_or(false)
}

fn has_remote(repo: &Repo) -> bool {
    repo.remotes().map(|r| !r.is_empty()).unwrap_or(false)
}

/// The path git resolves for a file inside `.git`, worktrees included.
fn git_path(repo: &Repo, name: &str) -> Option<PathBuf> {
    let rel = repo.git(&["rev-parse", "--git-path", name]).ok()?;
    let rel = rel.trim();
    if rel.is_empty() {
        return None;
    }
    let path = Path::new(rel);
    Some(if path.is_absolute() { path.to_path_buf() } else { repo.path().join(path) })
}

/// Whether `gh stack rebase` is stopped, waiting for a conflict to be resolved.
pub fn restack_in_progress(repo: &Repo) -> bool {
    git_path(repo, REBASE_STATE).map(|p| p.exists()).unwrap_or(false)
}

/// The branch a stopped restack is stuck on, from gh's own state file.
fn conflict_branch(repo: &Repo) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct State {
        conflict_branch: Option<String>,
    }
    let path = git_path(repo, REBASE_STATE)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<State>(&text).ok()?.conflict_branch.filter(|b| !b.is_empty())
}

// ---------------------------------------------------------------------------
// Reading the stack
// ---------------------------------------------------------------------------

/// The repository's default branch: `origin/HEAD` when it is known, else the
/// first of the usual names that exists, else whatever is checked out.
pub fn default_branch(repo: &Repo) -> String {
    if let Ok(out) = repo.git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = out.trim().strip_prefix("origin/") {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    let locals = local_branches(repo);
    for name in ["main", "master", "trunk"] {
        if locals.iter().any(|b| b == name) {
            return name.to_string();
        }
    }
    repo.current_branch()
}

fn local_branches(repo: &Repo) -> Vec<String> {
    repo.git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .map(|out| out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

/// `gh stack view --json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    trunk: String,
    #[serde(default)]
    current_branch: String,
    #[serde(default)]
    branches: Vec<ViewBranch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewBranch {
    name: String,
    #[serde(default)]
    is_current: bool,
    #[serde(default)]
    is_merged: bool,
    #[serde(default)]
    is_queued: bool,
    #[serde(default)]
    needs_rebase: bool,
    #[serde(default)]
    pr: Option<ViewPr>,
}

#[derive(Deserialize)]
struct ViewPr {
    number: u64,
    #[serde(default)]
    url: Option<String>,
}

/// gh's ways of saying "this branch is not in a stack", none of which is an
/// error for the view — an untracked branch is a normal thing to look at.
fn is_untracked_message(msg: &str) -> bool {
    msg.contains("is not part of a stack")
        || msg.contains("belongs to multiple stacks")
        || msg.contains("no stacks found")
        || msg.contains("No stacks")
}

/// The stack `branch` belongs to, bottom-first, as `gh stack` sees it.
///
/// An untracked branch comes back as an empty, untracked stack based on the
/// default branch — with the chain the previous implementation recorded, if
/// there is one, so it can be imported.
pub fn stack_for(repo: &Repo, branch: &str, auth: Option<&str>) -> Result<Stack> {
    let mut stack = Stack {
        trunk: default_branch(repo),
        restacking: restack_in_progress(repo),
        ..Default::default()
    };
    let run = run(repo, &["view", "--json"], auth)?;
    if run.code != 0 {
        let msg = run.error();
        // Mid-restack, HEAD is detached on the conflicted commit and gh
        // cannot say which stack that is. The stopped restack is the whole
        // story until it is continued or abandoned.
        if stack.restacking {
            stack.tracked = true;
            return Ok(stack);
        }
        if is_untracked_message(&msg) {
            stack.legacy = legacy_chain(repo, branch);
            return Ok(stack);
        }
        return Err(GitError::Command(msg));
    }
    let view: View = serde_json::from_str(run.stdout.trim()).map_err(|e| {
        GitError::Command(format!("Could not read `gh stack view --json`: {e}"))
    })?;
    if !view.trunk.is_empty() {
        stack.trunk = view.trunk.clone();
    }
    stack.tracked = true;

    let current = if view.current_branch.is_empty() {
        repo.current_branch()
    } else {
        view.current_branch.clone()
    };
    // A merged branch stays in gh's list (marked) until it is pruned. What
    // sits on it is, for every purpose that matters here, based on the
    // nearest branch below that has not merged — that is where GitHub
    // retargets its pull request, and what its diff is against.
    let mut parent = stack.trunk.clone();
    for (i, b) in view.branches.iter().enumerate() {
        let range = format!("{parent}..{}", b.name);
        let commits = repo.log(MAX_ENTRY_COMMITS, Some(&range)).unwrap_or_default();
        let merged = b.is_merged || landed(repo, &stack.trunk, &b.name, &parent);
        stack.entries.push(StackEntry {
            branch: b.name.clone(),
            parent: parent.clone(),
            commits,
            pr: b.pr.as_ref().map(|p| p.number),
            pr_url: b.pr.as_ref().and_then(|p| p.url.clone()),
            needs_restack: b.needs_rebase && !merged,
            merged,
            queued: b.is_queued,
        });
        if b.is_current || b.name == current {
            stack.current = Some(i);
        }
        if !merged {
            parent = b.name.clone();
        }
    }
    Ok(stack)
}

/// Whether everything this branch carries is already in the trunk.
///
/// A merge or fast-forward leaves the branch an ancestor of the trunk, so
/// `trunk..branch` is empty. A rebase or cherry-pick gives the same changes
/// new shas, and only `git cherry` sees through that: it compares patch ids,
/// marking with `-` every commit whose change is upstream already. Passing
/// `parent` as the limit confines the comparison to this entry's own commits.
///
/// A squash merge defeats both — one upstream commit whose patch matches no
/// single commit here — which is why gh's own answer, from the pull request,
/// is consulted first.
fn landed(repo: &Repo, trunk: &str, branch: &str, parent: &str) -> bool {
    let range = format!("{trunk}..{branch}");
    if let Ok(out) = repo.git(&["rev-list", "--count", &range]) {
        if out.trim() == "0" {
            return true;
        }
    }
    let Ok(out) = repo.git(&["cherry", trunk, branch, parent]) else {
        return false;
    };
    let mut any = false;
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        any = true;
        if !line.starts_with('-') {
            return false;
        }
    }
    any
}

// ---------------------------------------------------------------------------
// Building a stack
// ---------------------------------------------------------------------------

/// Puts the checked-out branch under `gh stack`'s care as a stack of one.
pub fn track(repo: &Repo, stack: &Stack, auth: Option<&str>) -> Result<String> {
    let current = repo.current_branch();
    if current.is_empty() || current == stack.trunk {
        return Err(GitError::Command(format!(
            "`{}` is the trunk. Check out a branch to track, or start one on top of it.",
            stack.trunk
        )));
    }
    run_ok(repo, &["init", "--base", &stack.trunk, &current], auth)?;
    Ok(format!("{current} is now a stack on {}.", stack.trunk))
}

/// Starts `name` on top of the stack and records it there.
///
/// On a tracked stack that is `gh stack add`, which only works from the top,
/// so the top is checked out first. On an untracked branch the branch and the
/// new one become a stack together; on the trunk the new branch starts one.
/// Either way the new branch ends up checked out.
pub fn branch_on_tip(
    repo: &Repo,
    stack: &Stack,
    name: &str,
    auth: Option<&str>,
) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(GitError::Command("A branch needs a name.".into()));
    }
    if local_branches(repo).iter().any(|b| b == name) {
        return Err(GitError::Command(format!(
            "A branch called {name} already exists. Start a new one, or restructure \
             the stack with `gh stack modify` to adopt it."
        )));
    }
    let parent;
    if stack.tracked {
        parent = stack.tip();
        if repo.current_branch() != parent {
            repo.checkout(&parent)?;
        }
        run_ok(repo, &["add", name], auth)?;
    } else {
        let current = repo.current_branch();
        if current.is_empty() || current == stack.trunk {
            parent = stack.trunk.clone();
            run_ok(repo, &["init", "--base", &stack.trunk, name], auth)?;
        } else {
            parent = current.clone();
            run_ok(repo, &["init", "--base", &stack.trunk, &current, name], auth)?;
        }
    }
    Ok(format!("Started {name} on top of {parent}."))
}

/// Forgets the stack locally. The branches, and any stack on GitHub, are left
/// alone.
pub fn untrack(repo: &Repo, auth: Option<&str>) -> Result<Vec<String>> {
    Ok(run_ok(repo, &["unstack", "--local"], auth)?.log())
}

// ---------------------------------------------------------------------------
// Restacking
// ---------------------------------------------------------------------------

/// What a restack did.
#[derive(Debug, Clone, Default)]
pub struct RestackReport {
    pub log: Vec<String>,
    /// Branches whose tip changed.
    pub moved: Vec<String>,
    /// Set when a rebase stopped on a conflict, naming the branch. The rebase
    /// is left in progress for the conflict resolver; finish with
    /// [`restack_continue`] or give up with [`restack_abort`].
    pub conflicted: Option<String>,
}

impl RestackReport {
    pub fn message(&self) -> String {
        if let Some(branch) = &self.conflicted {
            return format!(
                "Restack stopped on {branch} with conflicts. Resolve them, then continue \
                 the restack."
            );
        }
        match self.moved.len() {
            0 => "Stack is already in order.".to_string(),
            n => format!("Restacked {n} branch(es): {}.", self.moved.join(", ")),
        }
    }
}

/// Rebases every branch in the stack back on top of its parent, bottom-up.
///
/// `gh stack rebase` — which also fetches the trunk and fast-forwards it when
/// there is a remote to fetch from. Without one, only the branches are
/// rebased onto each other.
///
/// Refuses a dirty working tree rather than stashing on the user's behalf:
/// rebasing every branch in a stack is not the moment to find out what an
/// automatic stash did with a half-finished change. Untracked files are fine.
pub fn restack(repo: &Repo, stack: &Stack, auth: Option<&str>) -> Result<RestackReport> {
    if restack_in_progress(repo) {
        return Err(GitError::Command(
            "A restack is already stopped on a conflict. Continue or abort it first.".into(),
        ));
    }
    if stack.entries.is_empty() {
        return Ok(RestackReport::default());
    }
    if has_uncommitted_changes(repo)? {
        return Err(GitError::Command(
            "The working tree has uncommitted changes. Commit or stash them before \
             restacking: rebasing every branch in the stack needs a clean tree."
                .into(),
        ));
    }
    let tips = tips(repo, stack);
    let args: &[&str] = if has_remote(repo) { &["rebase"] } else { &["rebase", "--no-trunk"] };
    let run = run(repo, args, auth)?;
    finish_restack(repo, run, &tips)
}

/// Resumes a restack after its conflicts were resolved and staged.
///
/// Works whether the underlying `git rebase` was already continued (the
/// conflict resolver does that) or is still sitting on the conflict.
pub fn restack_continue(repo: &Repo, auth: Option<&str>) -> Result<RestackReport> {
    if !restack_in_progress(repo) {
        return Err(GitError::Command("No restack is waiting to be continued.".into()));
    }
    let run = run(repo, &["rebase", "--continue"], auth)?;
    finish_restack(repo, run, &BTreeMap::new())
}

/// Gives up on a stopped restack, putting every branch back where it was.
pub fn restack_abort(repo: &Repo, auth: Option<&str>) -> Result<Vec<String>> {
    Ok(run_ok(repo, &["rebase", "--abort"], auth)?.log())
}

fn finish_restack(
    repo: &Repo,
    run: Run,
    before: &BTreeMap<String, String>,
) -> Result<RestackReport> {
    let mut report = RestackReport { log: run.log(), ..Default::default() };
    if run.code == EXIT_CONFLICT || (run.code != 0 && restack_in_progress(repo)) {
        report.conflicted = conflict_branch(repo).or_else(|| Some("a branch".to_string()));
        return Ok(report);
    }
    if run.code != 0 {
        return Err(GitError::Command(run.error()));
    }
    for (branch, sha) in before {
        if rev_parse(repo, branch).ok().as_deref() != Some(sha.as_str()) {
            report.moved.push(branch.clone());
        }
    }
    Ok(report)
}

fn tips(repo: &Repo, stack: &Stack) -> BTreeMap<String, String> {
    stack
        .entries
        .iter()
        .filter_map(|e| rev_parse(repo, &e.branch).ok().map(|sha| (e.branch.clone(), sha)))
        .collect()
}

fn rev_parse(repo: &Repo, rev: &str) -> Result<String> {
    repo.git(&["rev-parse", rev]).map(|o| o.trim().to_string())
}

/// Anything staged or modified. Untracked files do not count: a rebase
/// carries them along untouched.
fn has_uncommitted_changes(repo: &Repo) -> Result<bool> {
    Ok(repo.status()?.files.iter().any(|f| {
        f.conflicted
            || f.index_status.is_some_and(|s| s != FileStatus::Untracked)
            || f.work_status.is_some_and(|s| s != FileStatus::Untracked)
    }))
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// Pushes every active branch in the stack, force-with-lease.
pub fn push_stack(repo: &Repo, auth: Option<&str>) -> Result<Vec<String>> {
    Ok(run_ok(repo, &["push"], auth)?.log())
}

/// What [`submit`] did.
#[derive(Debug, Clone, Default)]
pub struct SubmitReport {
    pub log: Vec<String>,
    /// Pull requests that did not exist before this submit.
    pub opened: usize,
    /// Branches with a pull request afterwards.
    pub total: usize,
}

impl SubmitReport {
    pub fn message(&self) -> String {
        match self.opened {
            0 => format!("Stack submitted: {} pull request(s) up to date.", self.total),
            n => format!("Stack submitted: {n} opened, {} in the stack.", self.total),
        }
    }
}

/// Publishes the stack: every branch pushed, a pull request for each branch
/// pointed at the one below it, and the stack linked on GitHub.
///
/// `gh stack submit --auto`: titles from the branches' commits, no editor.
/// New pull requests are drafts unless `ready` is set, which also flips
/// existing drafts in the stack to ready for review.
///
/// Refuses a stale stack outright. A branch that is behind its parent would
/// open a pull request whose diff includes the branch below it, which is the
/// one thing a stack exists to prevent — and restacking is a rebase, so it is
/// the user's decision, not something to slip into a submit.
pub fn submit(
    repo: &Repo,
    stack: &Stack,
    ready: bool,
    auth: Option<&str>,
) -> Result<SubmitReport> {
    if let Some(e) = stack.entries.iter().find(|e| e.needs_restack) {
        return Err(GitError::Command(format!(
            "Restack first: {} is behind {}. Its pull request would show the \
             changes below it as its own.",
            e.branch, e.parent
        )));
    }
    let mut args = vec!["submit", "--auto"];
    if ready {
        args.push("--open");
    }
    let run = run_ok(repo, &args, auth)?;
    let mut report = SubmitReport { log: run.log(), ..Default::default() };
    // Counted from the stack as it now stands rather than parsed out of the
    // output: gh's wording is its own to change.
    let after = stack_for(repo, &repo.current_branch(), auth)?;
    for e in &after.entries {
        if e.pr.is_some() {
            report.total += 1;
            if stack.entry(&e.branch).is_none_or(|b| b.pr.is_none()) {
                report.opened += 1;
            }
        }
    }
    Ok(report)
}

/// What [`sync`] did.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub log: Vec<String>,
    /// Branches now known to have merged.
    pub merged: Vec<String>,
}

impl SyncReport {
    pub fn message(&self) -> String {
        match self.merged.len() {
            0 => "Stack synced with the remote.".to_string(),
            n => format!("Stack synced: {n} branch(es) merged ({}).", self.merged.join(", ")),
        }
    }
}

/// Brings the stack back in line with the remote.
///
/// `gh stack sync`: fetches, fast-forwards the trunk, rebases the branches
/// onto their updated parents, pushes them (atomically, with lease), reads
/// each pull request's state back, and links the open ones into the stack on
/// GitHub. A merged branch stays in the list, marked, so what sat on it is
/// still known to be based on it; nothing is deleted.
///
/// A rebase conflict here restores every branch and reports it: resolving
/// conflicts is what [`restack`] is for.
pub fn sync(repo: &Repo, auth: Option<&str>) -> Result<SyncReport> {
    let run = run_ok(repo, &["sync"], auth)?;
    let mut report = SyncReport { log: run.log(), ..Default::default() };
    let after = stack_for(repo, &repo.current_branch(), auth)?;
    report.merged = after.entries.iter().filter(|e| e.merged).map(|e| e.branch.clone()).collect();
    Ok(report)
}

// ---------------------------------------------------------------------------
// Migration from the built-in implementation
// ---------------------------------------------------------------------------

/// The chain the previous implementation recorded for `branch`, bottom
/// first, from `branch.<name>.devdock-parent` in git config. Empty when
/// `branch` was never stacked that way.
pub fn legacy_chain(repo: &Repo, branch: &str) -> Vec<String> {
    let links = legacy_parents(repo);
    if links.is_empty() || branch.is_empty() {
        return Vec::new();
    }
    let trunk = default_branch(repo);
    let locals = local_branches(repo);
    let in_chain = links.contains_key(branch) || links.values().any(|p| p == branch);
    if !in_chain || branch == trunk {
        return Vec::new();
    }
    let mut chain = vec![branch.to_string()];
    let mut cur = branch.to_string();
    while let Some(parent) = links.get(&cur) {
        if parent == &trunk || !locals.contains(parent) || chain.contains(parent) {
            break;
        }
        chain.push(parent.clone());
        cur = parent.clone();
    }
    chain.reverse();
    let mut cur = branch.to_string();
    loop {
        let children: Vec<&String> = links
            .iter()
            .filter(|(child, parent)| **parent == cur && locals.contains(child))
            .map(|(child, _)| child)
            .collect();
        // A fork was never a stack; the chain stops there.
        let [child] = children[..] else { break };
        if chain.contains(child) {
            break;
        }
        chain.push(child.clone());
        cur = child.clone();
    }
    chain
}

fn legacy_parents(repo: &Repo) -> BTreeMap<String, String> {
    let pattern = format!(r"^branch\..*\.{LEGACY_PARENT_KEY}$");
    let Ok(out) = repo.git(&["config", "--get-regexp", &pattern]) else {
        return BTreeMap::new();
    };
    let mut links = BTreeMap::new();
    for line in out.lines() {
        let Some((key, value)) = line.split_once(' ') else { continue };
        let Some(inner) = key.strip_prefix("branch.") else { continue };
        let Some(child) = inner.strip_suffix(&format!(".{LEGACY_PARENT_KEY}")) else { continue };
        if !child.is_empty() && !value.trim().is_empty() {
            links.insert(child.to_string(), value.trim().to_string());
        }
    }
    links
}

/// Hands a chain the previous implementation recorded over to `gh stack`,
/// then forgets the old record. The checked-out branch stays checked out.
pub fn import_legacy(repo: &Repo, stack: &Stack, auth: Option<&str>) -> Result<String> {
    if stack.legacy.is_empty() {
        return Err(GitError::Command("Nothing to import.".into()));
    }
    let start = repo.current_branch();
    let mut args = vec!["init", "--base", stack.trunk.as_str()];
    args.extend(stack.legacy.iter().map(String::as_str));
    run_ok(repo, &args, auth)?;
    for branch in &stack.legacy {
        for key in [LEGACY_PARENT_KEY, LEGACY_PR_KEY] {
            let _ = repo.git(&["config", "--unset", &format!("branch.{branch}.{key}")]);
        }
    }
    // `init` checks out the top of the stack.
    if !start.is_empty() && repo.current_branch() != start {
        repo.checkout(&start)?;
    }
    Ok(format!("Imported {} branch(es) into gh stack.", stack.legacy.len()))
}

/// The command to restructure the stack by hand, for the terminal panel.
pub fn modify_command() -> &'static str {
    "gh stack modify"
}
