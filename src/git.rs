//! Typed, synchronous wrapper around the `git` command-line tool.
//!
//! The entry point is [`Repo`], a lightweight handle to a working directory:
//!
//! ```no_run
//! use git_manage::git::Repo;
//!
//! let repo = Repo::open("/path/to/project")?;
//! let status = repo.status()?;
//! println!("{} files changed on {}", status.files.len(), status.branch);
//! # Ok::<(), git_manage::git::GitError>(())
//! ```
//!
//! All operations shell out to `git`, so behaviour always matches the user's
//! installed git version and configuration (hooks, credentials, aliases).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors produced by git operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `git` exited with a non-zero status. Contains the trimmed stderr/stdout.
    #[error("{0}")]
    Command(String),
    /// The `git` binary could not be spawned or the filesystem failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The given path is not (inside) a git repository.
    #[error("not a git repository: {0}")]
    NotARepo(String),
}

/// Convenience alias used across this module.
pub type Result<T> = std::result::Result<T, GitError>;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single changed file reported by `git status`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    /// Previous path for renames/copies.
    pub orig_path: Option<String>,
    /// Status of the index side (staged), e.g. `modified`, `added`.
    pub index_status: Option<FileStatus>,
    /// Status of the worktree side (unstaged).
    pub work_status: Option<FileStatus>,
    pub staged: bool,
    pub unstaged: bool,
    pub conflicted: bool,
}

/// Normalized file status letters from `git status --porcelain`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Conflicted,
    Untracked,
    Ignored,
    Typechange,
}

impl FileStatus {
    fn from_porcelain(c: char) -> Option<Self> {
        match c {
            'M' => Some(Self::Modified),
            'A' => Some(Self::Added),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            'U' => Some(Self::Conflicted),
            '?' => Some(Self::Untracked),
            '!' => Some(Self::Ignored),
            'T' => Some(Self::Typechange),
            _ => None,
        }
    }
}

/// Snapshot of the working tree returned by [`Repo::status`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Status {
    pub files: Vec<FileEntry>,
    pub branch: String,
    /// Commits ahead of the upstream branch (or total unpushed commits when
    /// the branch has no upstream yet).
    pub ahead: u32,
    /// Commits behind the upstream branch.
    pub behind: u32,
    pub has_upstream: bool,
    /// Whether any remote is configured at all.
    pub has_remote: bool,
    pub state: RepoState,
}

/// Whether a multi-step operation (merge, rebase, ...) is in progress.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RepoState {
    Clean,
    Merging,
    Rebasing,
    CherryPicking,
}

/// A local or remote branch.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    /// ISO-8601 committer date of the branch tip.
    pub date: String,
    /// Subject line of the tip commit.
    pub subject: String,
    pub current: bool,
}

/// Local and remote branches, plus the current branch name.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BranchList {
    pub current: String,
    pub local: Vec<Branch>,
    pub remote: Vec<Branch>,
}

/// Outcome of a merge/rebase style operation that may hit conflicts.
///
/// These operations "fail" routinely as part of normal workflows, so they
/// return this type instead of an `Err`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpOutcome {
    pub ok: bool,
    pub conflict: bool,
    pub message: String,
}

impl OpOutcome {
    fn from(result: Result<String>) -> Self {
        match result {
            Ok(out) => Self { ok: true, conflict: false, message: out.trim().to_string() },
            Err(e) => {
                let message = e.to_string();
                Self { ok: false, conflict: message.to_lowercase().contains("conflict"), message }
            }
        }
    }
}

/// One commit from [`Repo::log`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub email: String,
    /// ISO-8601 author date.
    pub date: String,
    pub subject: String,
    pub body: String,
    pub parents: Vec<String>,
    /// Branch/tag names pointing at this commit (from %D), e.g.
    /// ["HEAD -> main", "origin/main", "tag: v1.0"]. Empty unless the
    /// log was fetched with decorations (log_all).
    #[serde(default)]
    pub refs: Vec<String>,
}

/// A conflicted file with all three stages plus the current working copy,
/// ready to feed a merge editor.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConflictFile {
    pub path: String,
    /// Common ancestor version (stage 1).
    pub base: Option<String>,
    /// Current-branch version (stage 2).
    pub ours: Option<String>,
    /// Incoming version (stage 3).
    pub theirs: Option<String>,
    /// Content currently on disk, including conflict markers.
    pub working: Option<String>,
}

/// How to resolve a single conflicted file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    /// Keep the current branch's version.
    Ours,
    /// Keep the incoming version.
    Theirs,
    /// Write the provided content.
    Manual(String),
}

/// A configured remote.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// One entry in the stash list.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StashEntry {
    pub index: u32,
    pub message: String,
}

/// One hunk of a unified diff, self-contained enough to apply as a patch.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hunk {
    /// `diff --git`/`---`/`+++` lines for the file, ending in a newline.
    pub file_header: String,
    /// The `@@` header line plus hunk body, ending in a newline.
    pub text: String,
    /// The `@@ -a,b +c,d @@` line for display.
    pub header: String,
}

/// A file change inside a commit.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitFileChange {
    pub path: String,
    pub status: FileStatus,
}

/// One line of `git blame` output.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlameLine {
    pub sha: String,
    pub author: String,
    pub line: String,
}

// ---------------------------------------------------------------------------
// Repo
// ---------------------------------------------------------------------------

/// Handle to a git repository on disk.
///
/// Cheap to clone; holds only the worktree root path.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    // -- construction -------------------------------------------------------

    /// Opens an existing repository. `path` may be anywhere inside the worktree.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(GitError::NotARepo(path.display().to_string()));
        }
        let out = run_git(path, &["rev-parse", "--show-toplevel"], &[])
            .map_err(|_| GitError::NotARepo(path.display().to_string()))?;
        Ok(Self { root: PathBuf::from(out.trim()) })
    }

    /// Initializes a new repository at `path`, creating directories as needed.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        run_git(path, &["init"], &[])?;
        Self::open(path)
    }

    /// Clones `url` into `dest` and opens the result.
    pub fn clone(url: &str, dest: impl AsRef<Path>) -> Result<Self> {
        let dest = dest.as_ref();
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let dest_str = dest.to_string_lossy();
        run_git(parent, &["clone", url, &dest_str], &[])?;
        Self::open(dest)
    }

    /// Worktree root.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Directory name of the worktree root, used as a display name.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Runs an arbitrary git command in this repository and returns stdout.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        run_git(&self.root, args, &[])
    }

    fn git_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
        run_git(&self.root, args, env)
    }

    // -- status -------------------------------------------------------------

    /// Full working-tree status: changed files, branch, ahead/behind, state.
    pub fn status(&self) -> Result<Status> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        let files = parse_porcelain(&out);
        let (mut ahead, behind, has_upstream) = self.ahead_behind();
        let has_remote = !self.remotes()?.is_empty();
        if !has_upstream {
            // No upstream yet: unpushed commits are those not reachable from
            // any remote-tracking branch (not the branch's full history).
            ahead = self
                .git(&["rev-list", "--count", "HEAD", "--not", "--remotes"])
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
        }
        Ok(Status {
            files,
            branch: self.current_branch(),
            ahead,
            behind,
            has_upstream,
            has_remote,
            state: self.state()?,
        })
    }

    /// Reports whether a merge/rebase/cherry-pick is in progress.
    pub fn state(&self) -> Result<RepoState> {
        let git_dir = self.git(&["rev-parse", "--git-dir"])?.trim().to_string();
        let git_dir = if Path::new(&git_dir).is_absolute() {
            PathBuf::from(git_dir)
        } else {
            self.root.join(git_dir)
        };
        let state = if git_dir.join("MERGE_HEAD").exists() {
            RepoState::Merging
        } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            RepoState::Rebasing
        } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
            RepoState::CherryPicking
        } else {
            RepoState::Clean
        };
        Ok(state)
    }

    fn ahead_behind(&self) -> (u32, u32, bool) {
        let Ok(out) = self.git(&["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
        else {
            return (0, 0, false);
        };
        let counts: Vec<u32> = out.split_whitespace().filter_map(|s| s.parse().ok()).collect();
        match counts.as_slice() {
            [behind, ahead] => (*ahead, *behind, true),
            _ => (0, 0, false),
        }
    }

    // -- staging ------------------------------------------------------------

    /// Stages the given paths.
    pub fn stage(&self, files: &[String]) -> Result<()> {
        self.run_on_files(&["add", "--"], files)
    }

    /// Unstages the given paths.
    pub fn unstage(&self, files: &[String]) -> Result<()> {
        self.run_on_files(&["reset", "HEAD", "--"], files)
    }

    /// Stages every change, including untracked files.
    pub fn stage_all(&self) -> Result<()> {
        self.git(&["add", "-A"]).map(drop)
    }

    /// Clears the index without touching the worktree.
    pub fn unstage_all(&self) -> Result<()> {
        self.git(&["reset", "HEAD"]).map(drop)
    }

    /// Discards changes: restores tracked files, deletes untracked ones.
    pub fn discard(&self, files: &[String]) -> Result<()> {
        let status = self.status()?;
        let untracked: HashSet<&str> = status
            .files
            .iter()
            .filter(|f| {
                f.index_status == Some(FileStatus::Untracked)
                    || f.work_status == Some(FileStatus::Untracked)
            })
            .map(|f| f.path.as_str())
            .collect();

        let tracked: Vec<String> = files
            .iter()
            .filter(|f| !untracked.contains(f.as_str()))
            .cloned()
            .collect();
        if !tracked.is_empty() {
            self.run_on_files(&["checkout", "--"], &tracked)?;
        }
        for file in files.iter().filter(|f| untracked.contains(f.as_str())) {
            let target = self.root.join(file);
            if target.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else if target.exists() {
                std::fs::remove_file(&target)?;
            }
        }
        Ok(())
    }

    fn run_on_files(&self, prefix: &[&str], files: &[String]) -> Result<()> {
        let mut args: Vec<&str> = prefix.to_vec();
        args.extend(files.iter().map(String::as_str));
        self.git(&args).map(drop)
    }

    // -- commits ------------------------------------------------------------

    /// Creates a commit from the index. Returns the new commit's SHA.
    ///
    /// `description`, when non-empty, becomes the commit body.
    pub fn commit(&self, summary: &str, description: &str, amend: bool) -> Result<String> {
        let message = if description.trim().is_empty() {
            summary.to_string()
        } else {
            format!("{summary}\n\n{description}")
        };
        let mut args = vec!["commit", "-m", &message];
        if amend {
            args.push("--amend");
        }
        self.git(&args)?;
        Ok(self.git(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// Commit history across all branches, newest first (for the graph).
    pub fn log_all(&self, limit: u32) -> Result<Vec<Commit>> {
        const FIELD: char = '\u{1f}';
        const RECORD: char = '\u{1e}';
        let format = format!(
            "--format=%H{FIELD}%h{FIELD}%an{FIELD}%ae{FIELD}%aI{FIELD}%s{FIELD}%b{FIELD}%P{FIELD}%D{RECORD}"
        );
        let max_count = format!("--max-count={limit}");
        let args =
            vec!["log", max_count.as_str(), format.as_str(), "--all", "--topo-order"];
        let Ok(out) = self.git(&args) else {
            return Ok(Vec::new());
        };
        Ok(out
            .split(RECORD)
            .filter(|r| !r.trim().is_empty())
            .filter_map(|record| parse_commit(record.trim_start_matches('\n'), FIELD))
            .collect())
    }

    /// Commit history, newest first.
    pub fn log(&self, limit: u32, branch: Option<&str>) -> Result<Vec<Commit>> {
        // Unit/record separators cannot appear in commit metadata.
        const FIELD: char = '\u{1f}';
        const RECORD: char = '\u{1e}';
        let format =
            format!("--format=%H{FIELD}%h{FIELD}%an{FIELD}%ae{FIELD}%aI{FIELD}%s{FIELD}%b{FIELD}%P{RECORD}");
        let max_count = format!("--max-count={limit}");
        let mut args = vec!["log", max_count.as_str(), format.as_str()];
        if let Some(branch) = branch {
            args.push(branch);
        }
        let Ok(out) = self.git(&args) else {
            return Ok(Vec::new()); // repository without commits
        };
        Ok(out
            .split(RECORD)
            .filter(|r| !r.trim().is_empty())
            .filter_map(|record| parse_commit(record.trim_start_matches('\n'), FIELD))
            .collect())
    }

    // -- diffs --------------------------------------------------------------

    /// Unified diff for one file. Untracked files diff against `/dev/null`.
    pub fn diff_file(&self, file: &str, staged: bool) -> Result<String> {
        if !staged && self.is_untracked(file)? {
            return self.diff_untracked(file);
        }
        let mut args = vec!["diff"];
        if staged {
            args.push("--cached");
        }
        args.extend(["--", file]);
        self.git(&args)
    }

    /// Unified diff of the whole tree (staged or unstaged side).
    pub fn diff_all(&self, staged: bool) -> Result<String> {
        if staged {
            self.git(&["diff", "--cached"])
        } else {
            self.git(&["diff"])
        }
    }

    /// Patch + stats for one commit, as shown in history views.
    pub fn diff_commit(&self, sha: &str) -> Result<String> {
        self.git(&["show", "--stat", "--patch", "--format=fuller", sha])
    }

    /// Best-effort diff for AI commit-message generation: staged changes,
    /// else unstaged changes, else untracked file contents (first 20 files).
    pub fn diff_for_ai(&self) -> Result<String> {
        let staged = self.git(&["diff", "--cached"])?;
        if !staged.trim().is_empty() {
            return Ok(staged);
        }
        let unstaged = self.git(&["diff"])?;
        if !unstaged.trim().is_empty() {
            return Ok(unstaged);
        }
        let status = self.status()?;
        let diffs: Vec<String> = status
            .files
            .iter()
            .filter(|f| f.work_status == Some(FileStatus::Untracked))
            .take(20)
            .filter_map(|f| self.diff_untracked(&f.path).ok())
            .collect();
        Ok(diffs.join("\n"))
    }

    fn is_untracked(&self, file: &str) -> Result<bool> {
        let status = self.status()?;
        Ok(status
            .files
            .iter()
            .any(|f| f.path == file && f.work_status == Some(FileStatus::Untracked)))
    }

    fn diff_untracked(&self, file: &str) -> Result<String> {
        // `git diff --no-index` exits 1 when files differ, so bypass run_git.
        let out = Command::new("git")
            .args(["diff", "--no-index", "--", "/dev/null", file])
            .current_dir(&self.root)
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    // -- branches -----------------------------------------------------------

    /// Current branch name, or a descriptive placeholder when detached/empty.
    pub fn current_branch(&self) -> String {
        if let Ok(out) = self.git(&["symbolic-ref", "--short", "HEAD"]) {
            return out.trim().to_string();
        }
        match self.git(&["rev-parse", "--short", "HEAD"]) {
            Ok(sha) => format!("(detached: {})", sha.trim()),
            Err(_) => "(no commits)".to_string(),
        }
    }

    /// All local and remote branches, sorted by most recent commit.
    pub fn branches(&self) -> Result<BranchList> {
        let current = self.current_branch();
        Ok(BranchList {
            local: self.list_refs("refs/heads", &current),
            remote: self.list_refs("refs/remotes", &current),
            current,
        })
    }

    fn list_refs(&self, namespace: &str, current: &str) -> Vec<Branch> {
        const FORMAT: &str =
            "--format=%(refname:short)\t%(objectname:short)\t%(committerdate:iso8601)\t%(subject)";
        let out = self
            .git(&["for-each-ref", "--sort=-committerdate", FORMAT, namespace])
            .unwrap_or_default();
        out.lines().filter_map(|line| parse_branch(line, current)).collect()
    }

    /// Creates a branch, optionally checking it out.
    pub fn create_branch(&self, name: &str, checkout: bool) -> Result<()> {
        let args: &[&str] =
            if checkout { &["checkout", "-b", name] } else { &["branch", name] };
        self.git(args).map(drop)
    }

    /// Checks out a branch (or any committish).
    pub fn checkout(&self, name: &str) -> Result<()> {
        self.git(&["checkout", name]).map(drop)
    }

    /// Deletes a local branch. `force` uses `-D`.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        self.git(&["branch", if force { "-D" } else { "-d" }, name]).map(drop)
    }

    // -- merge / rebase -----------------------------------------------------

    /// Merges `branch` into the current branch.
    pub fn merge(&self, branch: &str) -> OpOutcome {
        OpOutcome::from(self.git(&["merge", "--no-edit", branch]))
    }

    /// Merges the current branch into `target`: checks out `target`, then
    /// merges the previous branch into it. On success you end up on
    /// `target` with the merge applied (push it to publish).
    pub fn merge_into(&self, target: &str) -> OpOutcome {
        let source = self.current_branch();
        if source == target {
            return OpOutcome {
                ok: false,
                conflict: false,
                message: "Source and target are the same branch.".into(),
            };
        }
        if let Err(e) = self.checkout(target) {
            return OpOutcome { ok: false, conflict: false, message: e.to_string() };
        }
        let outcome = self.merge(&source);
        if outcome.ok {
            OpOutcome {
                ok: true,
                conflict: false,
                message: format!(
                    "Merged {source} into {target}. You are now on {target}; push to publish."
                ),
            }
        } else {
            outcome
        }
    }

    /// Aborts an in-progress merge.
    pub fn merge_abort(&self) -> Result<()> {
        self.git(&["merge", "--abort"]).map(drop)
    }

    /// Commits a resolved merge with the default message.
    pub fn merge_continue(&self) -> OpOutcome {
        OpOutcome::from(self.git_env(&["commit", "--no-edit"], &[("GIT_EDITOR", "true")]))
    }

    /// Rebases the current branch onto `onto`.
    pub fn rebase(&self, onto: &str) -> OpOutcome {
        OpOutcome::from(self.git(&["rebase", onto]))
    }

    /// Continues a rebase after conflicts were staged.
    pub fn rebase_continue(&self) -> OpOutcome {
        OpOutcome::from(self.git_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")]))
    }

    /// Aborts an in-progress rebase.
    pub fn rebase_abort(&self) -> Result<()> {
        self.git(&["rebase", "--abort"]).map(drop)
    }

    // -- conflicts ----------------------------------------------------------

    /// All conflicted files with base/ours/theirs/working contents.
    pub fn conflicts(&self) -> Result<Vec<ConflictFile>> {
        let status = self.status()?;
        Ok(status
            .files
            .iter()
            .filter(|f| f.conflicted)
            .map(|f| ConflictFile {
                base: self.show_stage(1, &f.path),
                ours: self.show_stage(2, &f.path),
                theirs: self.show_stage(3, &f.path),
                working: std::fs::read_to_string(self.root.join(&f.path)).ok(),
                path: f.path.clone(),
            })
            .collect())
    }

    fn show_stage(&self, stage: u8, path: &str) -> Option<String> {
        self.git(&["show", &format!(":{stage}:{path}")]).ok()
    }

    /// Resolves one conflicted file and stages the result.
    pub fn resolve(&self, file: &str, resolution: &Resolution) -> Result<()> {
        match resolution {
            Resolution::Ours => self.git(&["checkout", "--ours", "--", file]).map(drop)?,
            Resolution::Theirs => self.git(&["checkout", "--theirs", "--", file]).map(drop)?,
            Resolution::Manual(content) => std::fs::write(self.root.join(file), content)?,
        }
        self.git(&["add", "--", file]).map(drop)
    }

    // -- remotes ------------------------------------------------------------

    /// Configured remotes (deduplicated fetch/push pairs).
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        let out = self.git(&["remote", "-v"]).unwrap_or_default();
        let mut seen = HashSet::new();
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let name = parts.next()?;
                let url = parts.next()?;
                seen.insert(name.to_string())
                    .then(|| Remote { name: name.to_string(), url: url.to_string() })
            })
            .collect())
    }

    /// Adds (or replaces) a named remote.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        if self.remotes()?.iter().any(|r| r.name == name) {
            self.git(&["remote", "set-url", name, url]).map(drop)
        } else {
            self.git(&["remote", "add", name, url]).map(drop)
        }
    }

    /// Fetches all remotes, pruning removed branches.
    ///
    /// `auth` supplies a GitHub token used for github.com HTTPS remotes.
    /// Operations always go through the named remote so tracking refs
    /// (`refs/remotes/origin/*`) and ahead/behind counts stay correct.
    pub fn fetch(&self, auth: Option<&str>) -> Result<()> {
        self.git_auth(&["fetch", "--all", "--prune"], auth).map(drop)
    }

    /// Pulls the current branch. See [`Repo::fetch`] for `auth`.
    pub fn pull(&self, auth: Option<&str>) -> Result<String> {
        Ok(self.git_auth(&["pull", "--no-edit"], auth)?.trim().to_string())
    }

    /// Pushes the current branch. See [`Repo::fetch`] for `auth`.
    pub fn push(&self, set_upstream: bool, auth: Option<&str>) -> Result<String> {
        self.push_inner(set_upstream, false, auth)
    }

    /// Force-pushes with `--force-with-lease` (safe force: fails if the
    /// remote moved since the last fetch). Needed after amend/rebase of
    /// already-pushed commits.
    pub fn force_push(&self, auth: Option<&str>) -> Result<String> {
        self.push_inner(false, true, auth)
    }

    fn push_inner(&self, set_upstream: bool, force: bool, auth: Option<&str>) -> Result<String> {
        let branch = self.current_branch();
        let mut args: Vec<&str> = vec!["push"];
        if force {
            args.push("--force-with-lease");
        }
        if set_upstream {
            args.extend(["--set-upstream", "origin", &branch]);
        }
        let out = self.git_auth(&args, auth)?;
        Ok(out.trim().to_string())
    }

    /// Runs a git command, injecting a GitHub token as an HTTP Basic header
    /// via `-c http.<url>.extraheader` when `auth` is provided. This keeps
    /// the named remote in use (so tracking refs update) and never writes
    /// the token to disk or the remote URL.
    fn git_auth(&self, args: &[&str], auth: Option<&str>) -> Result<String> {
        match auth {
            Some(token) if self.origin_is_github() => {
                use base64::Engine as _;
                let basic = base64::engine::general_purpose::STANDARD
                    .encode(format!("x-access-token:{token}"));
                let header =
                    format!("http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}");
                let mut full: Vec<&str> = vec!["-c", &header];
                full.extend(args);
                self.git(&full)
            }
            _ => self.git(args),
        }
    }

    /// Whether `origin` points at github.com over HTTPS (token auth applies).
    fn origin_is_github(&self) -> bool {
        self.git(&["remote", "get-url", "origin"])
            .map(|url| url.trim().starts_with("https://github.com/"))
            .unwrap_or(false)
    }

    // -- stash --------------------------------------------------------------

    /// Stashes all local changes (including untracked files).
    pub fn stash_save(&self, message: &str) -> Result<()> {
        let msg = if message.trim().is_empty() { "git-manage stash" } else { message };
        self.git(&["stash", "push", "--include-untracked", "-m", msg]).map(drop)
    }

    /// Stash entries, newest first: `(index, message)`.
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let out = self.git(&["stash", "list", "--format=%gd\x1f%gs"]).unwrap_or_default();
        Ok(out
            .lines()
            .filter_map(|line| {
                let (rev, message) = line.split_once('\u{1f}')?;
                let index = rev.strip_prefix("stash@{")?.strip_suffix('}')?.parse().ok()?;
                Some(StashEntry { index, message: message.to_string() })
            })
            .collect())
    }

    /// Applies and removes the given stash entry.
    pub fn stash_pop(&self, index: u32) -> Result<()> {
        self.git(&["stash", "pop", &format!("stash@{{{index}}}")]).map(drop)
    }

    /// Deletes the given stash entry without applying it.
    pub fn stash_drop(&self, index: u32) -> Result<()> {
        self.git(&["stash", "drop", &format!("stash@{{{index}}}")]).map(drop)
    }

    // -- undo ---------------------------------------------------------------

    /// Undoes the last commit, keeping its changes staged (soft reset).
    /// Refuses when the commit has already been pushed to the upstream.
    pub fn undo_last_commit(&self) -> Result<()> {
        if let Ok(upstream) = self.git(&["rev-parse", "@{upstream}"]) {
            let head = self.git(&["rev-parse", "HEAD"])?;
            let merged = self
                .git(&["merge-base", "--is-ancestor", "HEAD", upstream.trim()])
                .is_ok();
            if merged && upstream.trim() == head.trim() {
                return Err(GitError::Command(
                    "Last commit is already pushed. Revert it instead.".into(),
                ));
            }
        }
        self.git(&["reset", "--soft", "HEAD~1"]).map(drop)
    }

    /// Reverts a commit by creating an inverse commit. Safe for pushed
    /// history, unlike undo.
    pub fn revert_commit(&self, sha: &str) -> OpOutcome {
        OpOutcome::from(self.git_env(&["revert", "--no-edit", sha], &[("GIT_EDITOR", "true")]))
    }

    /// Progress of an in-progress rebase: `(done, total)` commits applied.
    pub fn rebase_progress(&self) -> Option<(u32, u32)> {
        let git_dir = self.git(&["rev-parse", "--git-dir"]).ok()?;
        let git_dir = git_dir.trim();
        let base = if Path::new(git_dir).is_absolute() {
            PathBuf::from(git_dir)
        } else {
            self.root.join(git_dir)
        };
        let dir = ["rebase-merge", "rebase-apply"]
            .iter()
            .map(|d| base.join(d))
            .find(|p| p.exists())?;
        let read_num = |name: &str| -> Option<u32> {
            std::fs::read_to_string(dir.join(name)).ok()?.trim().parse().ok()
        };
        Some((read_num("msgnum")?, read_num("end")?))
    }

    // -- hunks / partial staging ---------------------------------------------

    /// Parses the unstaged diff of one file into hunks for partial staging.
    pub fn hunks(&self, file: &str) -> Result<Vec<Hunk>> {
        let diff = self.diff_file(file, false)?;
        Ok(parse_hunks(&diff))
    }

    /// Stages exactly one hunk of a file by applying a minimal patch to the
    /// index. `hunk` must come from [`Repo::hunks`] for the same file state.
    pub fn stage_hunk(&self, hunk: &Hunk) -> Result<()> {
        let patch = format!("{}{}", hunk.file_header, hunk.text);
        self.apply_cached(&patch)
    }

    /// Stages only the selected changed lines of a hunk.
    ///
    /// `selected` holds indices into the hunk body (lines after the `@@`
    /// header) for the `+`/`-` lines to keep. Unselected additions are
    /// dropped; unselected deletions become context.
    pub fn stage_lines(&self, hunk: &Hunk, selected: &[usize]) -> Result<()> {
        let patch = build_partial_patch(hunk, selected)
            .ok_or_else(|| GitError::Command("No lines selected".into()))?;
        self.apply_cached(&patch)
    }

    fn apply_cached(&self, patch: &str) -> Result<()> {
        let mut child = Command::new("git")
            .args(["apply", "--cached", "--unidiff-zero", "--recount", "-"])
            .current_dir(&self.root)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        use std::io::Write as _;
        child.stdin.take().expect("stdin piped").write_all(patch.as_bytes())?;
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(GitError::Command(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }

    /// Whether git treats the file as binary (no textual diff).
    pub fn is_binary(&self, file: &str) -> bool {
        self.git(&["diff", "--numstat", "--", file])
            .map(|out| out.lines().any(|l| l.starts_with("-\t-\t")))
            .unwrap_or(false)
    }

    // -- history details ------------------------------------------------------

    /// Files touched by a commit with their change status.
    pub fn commit_files(&self, sha: &str) -> Result<Vec<CommitFileChange>> {
        let out = self.git(&["show", "--name-status", "--format=", sha])?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let status = parts.next()?.chars().next()?;
                let path = parts.next_back()?.to_string();
                Some(CommitFileChange { status: FileStatus::from_porcelain(status)?, path })
            })
            .collect())
    }

    /// Patch for a single file within a commit.
    pub fn diff_commit_file(&self, sha: &str, file: &str) -> Result<String> {
        self.git(&["show", "--patch", "--format=", sha, "--", file])
    }

    // -- tags ----------------------------------------------------------------

    /// Tags, newest first.
    pub fn tags(&self) -> Result<Vec<String>> {
        let out = self.git(&["tag", "--sort=-creatordate"]).unwrap_or_default();
        Ok(out.lines().map(String::from).collect())
    }

    /// Creates an annotated tag at HEAD.
    pub fn create_tag(&self, name: &str, message: &str) -> Result<()> {
        let msg = if message.trim().is_empty() { name } else { message };
        self.git(&["tag", "-a", name, "-m", msg]).map(drop)
    }

    /// Pushes one tag to origin.
    pub fn push_tag(&self, name: &str, auth: Option<&str>) -> Result<()> {
        self.git_auth(&["push", "origin", name], auth).map(drop)
    }

    // -- misc ----------------------------------------------------------------

    /// Renames a local branch.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        self.git(&["branch", "-m", old, new]).map(drop)
    }

    /// Appends a pattern to the repository's .gitignore.
    pub fn ignore(&self, pattern: &str) -> Result<()> {
        use std::io::Write as _;
        let path = self.root.join(".gitignore");
        let needs_newline = std::fs::read_to_string(&path)
            .map(|s| !s.is_empty() && !s.ends_with('\n'))
            .unwrap_or(false);
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        if needs_newline {
            writeln!(f)?;
        }
        writeln!(f, "{pattern}")?;
        Ok(())
    }

    /// Per-line blame for a file: `(commit short sha, author, line)`.
    pub fn blame(&self, file: &str) -> Result<Vec<BlameLine>> {
        let out = self.git(&["blame", "--line-porcelain", "--", file])?;
        let mut lines = Vec::new();
        let mut sha = String::new();
        let mut author = String::new();
        for l in out.lines() {
            if let Some(rest) = l.strip_prefix("author ") {
                author = rest.to_string();
            } else if let Some(stripped) = l.strip_prefix('\t') {
                lines.push(BlameLine {
                    sha: sha.chars().take(7).collect(),
                    author: author.clone(),
                    line: stripped.to_string(),
                });
            } else if !l.starts_with(' ') {
                if let Some(first) = l.split(' ').next() {
                    if first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                        sha = first.to_string();
                    }
                }
            }
        }
        Ok(lines)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn run_git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let message = if stderr.trim().is_empty() { stdout } else { stderr };
        Err(GitError::Command(message.trim().to_string()))
    }
}

fn parse_porcelain(out: &str) -> Vec<FileEntry> {
    let entries: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
    let mut files = Vec::new();
    let mut iter = entries.iter().peekable();
    while let Some(entry) = iter.next() {
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else { continue };
        let path = entry[3..].to_string();
        // Renames/copies are followed by the original path as its own record.
        let orig_path = matches!(x, 'R' | 'C')
            .then(|| iter.next().map(|s| s.to_string()))
            .flatten();
        files.push(FileEntry {
            path,
            orig_path,
            index_status: FileStatus::from_porcelain(x),
            work_status: FileStatus::from_porcelain(y),
            staged: x != ' ' && x != '?',
            unstaged: y != ' ',
            conflicted: x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D'),
        });
    }
    files
}

fn parse_branch(line: &str, current: &str) -> Option<Branch> {
    let mut parts = line.splitn(4, '\t');
    let name = parts.next()?.to_string();
    if name.is_empty() || name.ends_with("/HEAD") {
        return None;
    }
    Some(Branch {
        current: name == current,
        name,
        sha: parts.next().unwrap_or_default().to_string(),
        date: parts.next().unwrap_or_default().to_string(),
        subject: parts.next().unwrap_or_default().to_string(),
    })
}

/// Splits a single-file unified diff into hunks that can be applied
/// independently with `git apply --cached`.
fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut file_header = String::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(Hunk {
                file_header: file_header.clone(),
                header: line.to_string(),
                text: format!("{line}\n"),
            });
        } else if let Some(h) = current.as_mut() {
            h.text.push_str(line);
            h.text.push('\n');
        } else {
            file_header.push_str(line);
            file_header.push('\n');
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    // Fix up headers captured before the first hunk for all hunks.
    for h in &mut hunks {
        h.file_header = file_header.clone();
    }
    hunks
}

/// Builds a patch containing only the selected changed lines of `hunk`.
///
/// `selected` indexes the hunk body lines (excluding the `@@` header).
/// Unselected `+` lines are omitted; unselected `-` lines turn into context.
/// Returns `None` when no changed line is selected. Uses `--recount`-friendly
/// output, so header line counts need not be adjusted.
fn build_partial_patch(hunk: &Hunk, selected: &[usize]) -> Option<String> {
    let selected: std::collections::HashSet<usize> = selected.iter().copied().collect();
    let mut body_lines: Vec<String> = Vec::new();
    let mut any_change = false;

    for (i, line) in hunk.text.lines().skip(1).enumerate() {
        let first = line.chars().next().unwrap_or(' ');
        match first {
            '+' if selected.contains(&i) => {
                any_change = true;
                body_lines.push(line.to_string());
            }
            '+' => { /* unselected addition: drop */ }
            '-' if selected.contains(&i) => {
                any_change = true;
                body_lines.push(line.to_string());
            }
            '-' => {
                // Unselected deletion: keep the line as context.
                body_lines.push(format!(" {}", &line[1..]));
            }
            _ => body_lines.push(line.to_string()),
        }
    }
    if !any_change {
        return None;
    }
    let header = hunk.text.lines().next()?.to_string();
    Some(format!("{}{}\n{}\n", hunk.file_header, header, body_lines.join("\n")))
}

fn parse_commit(record: &str, field_sep: char) -> Option<Commit> {
    let parts: Vec<&str> = record.split(field_sep).collect();
    // 8 fields (plain log) or 9 (decorated log with %D refs).
    let (core, refs_raw): (&[&str], &str) = match parts.as_slice() {
        p @ [_, _, _, _, _, _, _, _] => (p, ""),
        [p @ .., refs] if p.len() == 8 => (p, refs),
        _ => return None,
    };
    let [sha, short_sha, author, email, date, subject, body, parents] = core else {
        return None;
    };
    Some(Commit {
        sha: sha.to_string(),
        short_sha: short_sha.to_string(),
        author: author.to_string(),
        email: email.to_string(),
        date: date.to_string(),
        subject: subject.to_string(),
        body: body.trim().to_string(),
        parents: parents.split_whitespace().map(String::from).collect(),
        refs: refs_raw
            .split(", ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_untracked_and_modified() {
        let out = "?? new.txt\0 M changed.txt\0";
        let files = parse_porcelain(out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].work_status, Some(FileStatus::Untracked));
        assert!(!files[0].staged);
        assert_eq!(files[1].work_status, Some(FileStatus::Modified));
        assert!(!files[1].staged);
    }

    #[test]
    fn porcelain_parses_rename_with_orig_path() {
        let out = "R  new-name.txt\0old-name.txt\0";
        let files = parse_porcelain(out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].orig_path.as_deref(), Some("old-name.txt"));
        assert!(files[0].staged);
    }

    #[test]
    fn porcelain_flags_conflicts() {
        let out = "UU both.txt\0AA added-both.txt\0";
        let files = parse_porcelain(out);
        assert!(files.iter().all(|f| f.conflicted));
    }

    #[test]
    fn branch_line_parses() {
        let b = parse_branch("main\tabc123\t2026-01-01 00:00:00 +0000\tinitial", "main").unwrap();
        assert!(b.current);
        assert_eq!(b.sha, "abc123");
        assert_eq!(b.subject, "initial");
    }

    #[test]
    fn branch_head_pointer_is_skipped() {
        assert!(parse_branch("origin/HEAD\tabc\tdate\tsubj", "main").is_none());
    }
}
