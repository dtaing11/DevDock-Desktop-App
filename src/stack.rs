//! Stacked pull requests: a chain of branches, each based on the one below.
//!
//! A stack is how one large change gets reviewed as several small ones. Rather
//! than a single branch off `main` carrying twelve commits, each self-contained
//! piece gets its own branch and its own pull request, and each PR targets the
//! branch below it instead of the trunk. Reviewers see one focused diff per PR;
//! GitHub shows each PR the changes it actually introduces, because the ones
//! underneath are already in its base.
//!
//! GitHub has no "stack" object — a stack *is* the chain of base branches, and
//! that is exactly what this module manages:
//!
//! - the parent of each branch, recorded in git config as
//!   `branch.<name>.devdock-parent` (per-repository, survives everything, and
//!   is invisible to anyone who does not use this app);
//! - restacking: when a branch low in the stack changes, everything above it
//!   is rebased back on top, bottom-up, so the chain stays linear;
//! - submitting: pushing every branch and opening each PR against its parent
//!   (the [`nav_section`] block that goes in each PR body is built here too);
//! - syncing after a merge: the merged branch drops out and its children are
//!   re-parented onto what it was based on.
//!
//! Stacks are modelled as a straight line. Branches can of course fork off the
//! same parent, and [`Stack::forks`] reports it when they do, but the chain
//! itself is linear — restacking a tree in one action is a good way to lose
//! track of what moved where.

use std::collections::{BTreeMap, HashSet};

use crate::git::{Commit, GitError, Repo, Result};

/// Config key suffix under `branch.<name>.` holding the parent branch.
const PARENT_KEY: &str = "devdock-parent";

/// How many commits of one entry are loaded for display.
const MAX_ENTRY_COMMITS: u32 = 100;

/// Marker opening the generated stack block in a PR body.
pub const NAV_START: &str = "<!-- devdock-stack -->";
/// Marker closing the generated stack block in a PR body.
pub const NAV_END: &str = "<!-- /devdock-stack -->";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One branch in a stack, together with the branch it sits on.
#[derive(Debug, Clone)]
pub struct StackEntry {
    pub branch: String,
    /// The branch this one is based on: the base of its pull request.
    pub parent: String,
    /// Commits this entry adds on top of its parent (`parent..branch`).
    pub commits: Vec<Commit>,
    /// Number of the open pull request for this branch, once one exists.
    /// Filled in by the caller from GitHub; the model itself is offline.
    pub pr: Option<u64>,
    /// The parent has commits this branch does not: it must be rebased
    /// before its pull request shows the right diff.
    pub needs_restack: bool,
    /// Every commit of this entry is already in the trunk (by patch id), so
    /// the entry has landed and can leave the stack.
    pub merged: bool,
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
    /// Entries ordered bottom → top.
    pub entries: Vec<StackEntry>,
    /// Index of the checked-out branch within `entries`, if it is in the stack.
    pub current: Option<usize>,
    /// Branches that share a parent with an entry, so are not part of this
    /// chain. Reported rather than silently swallowed.
    pub forks: Vec<String>,
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
// Parent bookkeeping
// ---------------------------------------------------------------------------

fn parent_key(branch: &str) -> String {
    format!("branch.{branch}.{PARENT_KEY}")
}

/// The branch `branch` is stacked on, if it has been tracked.
pub fn parent_of(repo: &Repo, branch: &str) -> Option<String> {
    repo.git(&["config", "--get", &parent_key(branch)])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// Records `parent` as the branch `branch` is stacked on.
///
/// Refuses to close a loop: a stack whose parent links cycle would make
/// restacking rebase forever.
pub fn set_parent(repo: &Repo, branch: &str, parent: &str) -> Result<()> {
    if branch == parent {
        return Err(GitError::Command(format!("{branch} cannot be based on itself")));
    }
    let links = parents(repo);
    let mut walk = Some(parent.to_string());
    let mut seen = HashSet::new();
    while let Some(cur) = walk {
        if cur == branch {
            return Err(GitError::Command(format!(
                "{parent} is already stacked on {branch}; that would make a loop"
            )));
        }
        if !seen.insert(cur.clone()) {
            break;
        }
        walk = links.get(&cur).cloned();
    }
    repo.git(&["config", &parent_key(branch), parent]).map(drop)
}

/// Removes `branch` from the stack it was tracked in.
pub fn clear_parent(repo: &Repo, branch: &str) -> Result<()> {
    // `--unset` exits 5 when the key is already absent, which is not a failure
    // for a caller that just wants the branch untracked.
    match repo.git(&["config", "--unset", &parent_key(branch)]) {
        Ok(_) | Err(GitError::Command(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Every recorded parent link in the repository, child → parent.
pub fn parents(repo: &Repo) -> BTreeMap<String, String> {
    let pattern = format!("^branch\\..*\\.{PARENT_KEY}$");
    let Ok(out) = repo.git(&["config", "--get-regexp", &pattern]) else {
        return BTreeMap::new(); // exit 1 == no matches
    };
    out.lines()
        .filter_map(|line| {
            let (key, parent) = line.split_once(' ')?;
            let child = key
                .strip_prefix("branch.")?
                .strip_suffix(&format!(".{PARENT_KEY}"))?;
            (!child.is_empty() && !parent.is_empty())
                .then(|| (child.to_string(), parent.trim().to_string()))
        })
        .collect()
}

/// Config key suffix under `branch.<name>.` holding the pull request number.
const PR_KEY: &str = "devdock-pr";

fn pr_key(branch: &str) -> String {
    format!("branch.{branch}.{PR_KEY}")
}

/// The pull request opened for `branch`, if this app opened one.
///
/// Remembering the number is what makes a merge visible: GitHub's list
/// endpoint returns open pull requests only, so a stack that has forgotten
/// the number of the PR at its bottom cannot tell "merged" from "never
/// existed", and both look the same from the branch alone.
pub fn pr_of(repo: &Repo, branch: &str) -> Option<u64> {
    repo.git(&["config", "--get", &pr_key(branch)])
        .ok()
        .and_then(|out| out.trim().parse().ok())
}

/// Records the pull request opened for `branch`.
pub fn set_pr(repo: &Repo, branch: &str, number: u64) -> Result<()> {
    repo.git(&["config", &pr_key(branch), &number.to_string()]).map(drop)
}

/// Forgets the pull request recorded for `branch`.
pub fn clear_pr(repo: &Repo, branch: &str) -> Result<()> {
    match repo.git(&["config", "--unset", &pr_key(branch)]) {
        Ok(_) | Err(GitError::Command(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The repository's trunk: what a stack is ultimately based on.
///
/// `origin/HEAD` is the remote's own answer to the question, so it wins when
/// it is set. Otherwise fall back to the conventional names, and finally to
/// whatever is checked out, so a repository with neither still works.
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

// ---------------------------------------------------------------------------
// Building a stack
// ---------------------------------------------------------------------------

/// The stack `branch` belongs to, bottom-first.
///
/// An untracked branch is a stack of one based on the trunk: that is what it
/// is, and it means "add a branch on top" works without a separate step to
/// start a stack.
pub fn stack_for(repo: &Repo, branch: &str) -> Result<Stack> {
    let trunk = default_branch(repo);
    let locals: HashSet<String> = local_branches(repo).into_iter().collect();
    let mut stack = Stack { trunk: trunk.clone(), ..Default::default() };
    // The trunk is not an entry; it is what entries are based on. Neither is
    // a detached HEAD or a repository with no commits — `current_branch`
    // describes those in words, and there is no branch to stack.
    if branch.is_empty() || branch == trunk || !locals.contains(branch) {
        return Ok(stack);
    }
    let links = parents(repo);

    // Down to the trunk.
    let mut chain = vec![branch.to_string()];
    let mut seen: HashSet<String> = chain.iter().cloned().collect();
    let mut cur = branch.to_string();
    while let Some(parent) = links.get(&cur) {
        if parent == &trunk || !locals.contains(parent) || !seen.insert(parent.clone()) {
            break;
        }
        chain.push(parent.clone());
        cur = parent.clone();
    }
    chain.reverse();

    // Up through children. A branch with two children forks the chain; the
    // fork is reported and the walk stops rather than picking one at random.
    let mut cur = branch.to_string();
    loop {
        let children: Vec<String> = links
            .iter()
            .filter(|(child, parent)| **parent == cur && locals.contains(*child))
            .map(|(child, _)| child.clone())
            .collect();
        match children.len() {
            1 => {
                let child = children.into_iter().next().unwrap();
                if !seen.insert(child.clone()) {
                    break;
                }
                chain.push(child.clone());
                cur = child;
            }
            0 => break,
            _ => {
                stack.forks = children;
                stack.forks.sort();
                break;
            }
        }
    }

    let current = repo.current_branch();
    for (i, name) in chain.iter().enumerate() {
        let parent =
            if i == 0 { trunk.clone() } else { chain[i - 1].clone() };
        stack.entries.push(entry(repo, name, &parent, &trunk)?);
        if *name == current {
            stack.current = Some(i);
        }
    }
    Ok(stack)
}

fn entry(repo: &Repo, branch: &str, parent: &str, trunk: &str) -> Result<StackEntry> {
    let range = format!("{parent}..{branch}");
    let commits = repo.log(MAX_ENTRY_COMMITS, Some(&range)).unwrap_or_default();
    let needs_restack =
        repo.git(&["merge-base", "--is-ancestor", parent, branch]).is_err();
    Ok(StackEntry {
        branch: branch.to_string(),
        parent: parent.to_string(),
        merged: landed(repo, trunk, branch, parent),
        commits,
        pr: pr_of(repo, branch),
        needs_restack,
    })
}

/// Whether everything this branch carries is already in the trunk.
///
/// Two ways that happens. A merge or fast-forward leaves the branch an
/// ancestor of the trunk, so `trunk..branch` is empty. A rebase or
/// cherry-pick gives the same changes new shas, and only `git cherry` sees
/// through that: it compares patch ids, marking with `-` every commit whose
/// change is upstream already. Passing `parent` as the limit confines the
/// comparison to this entry's own commits.
///
/// A squash merge defeats both — one upstream commit whose patch matches no
/// single commit here — so a caller that knows the pull request's state
/// should trust that over this.
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
// Restacking
// ---------------------------------------------------------------------------

/// What one branch did during a restack.
#[derive(Debug, Clone)]
pub struct RestackStep {
    pub branch: String,
    /// False when the branch was already on top of its parent.
    pub moved: bool,
}

/// The outcome of restacking a whole stack.
#[derive(Debug, Clone, Default)]
pub struct RestackReport {
    pub steps: Vec<RestackStep>,
    /// The branch whose rebase stopped in conflict, if one did. The rebase is
    /// left in progress so the conflict resolver can finish it.
    pub conflicted: Option<String>,
}

impl RestackReport {
    pub fn moved(&self) -> Vec<&str> {
        self.steps.iter().filter(|s| s.moved).map(|s| s.branch.as_str()).collect()
    }

    pub fn message(&self) -> String {
        if let Some(branch) = &self.conflicted {
            return format!("Restack stopped: {branch} has conflicts.");
        }
        match self.moved().len() {
            0 => "Stack is already in order.".to_string(),
            n => format!("Restacked {n} branch(es): {}.", self.moved().join(", ")),
        }
    }
}

/// Rebases every branch in the stack back on top of its parent, bottom-up.
///
/// The subtlety is which commits to replay. Once the bottom branch is rebased
/// its children point at commits whose parent no longer exists in the chain,
/// and `git merge-base` then finds the *trunk* as the common ancestor — so a
/// naive rebase replays the branch below's commits a second time. Every
/// branch's tip is therefore recorded up front, before anything moves, and
/// each rebase replays exactly `<old parent tip>..<branch>`.
pub fn restack(repo: &Repo, stack: &Stack) -> Result<RestackReport> {
    let mut report = RestackReport::default();
    if stack.entries.is_empty() {
        return Ok(report);
    }
    if !repo.status()?.files.is_empty() {
        return Err(GitError::Command(
            "The working tree has uncommitted changes. Commit or stash them before \
             restacking: rebasing every branch in the stack needs a clean tree."
                .into(),
        ));
    }
    let start = repo.current_branch();

    // Tips as they are now, before any rebase rewrites them.
    let mut old_tips: Vec<String> = Vec::new();
    for e in &stack.entries {
        old_tips.push(rev_parse(repo, &e.branch)?);
    }

    for (i, e) in stack.entries.iter().enumerate() {
        // Already contains its parent: nothing to replay.
        if repo.git(&["merge-base", "--is-ancestor", &e.parent, &e.branch]).is_ok() {
            report.steps.push(RestackStep { branch: e.branch.clone(), moved: false });
            continue;
        }
        let upstream = if i == 0 {
            fork_point(repo, &e.parent, &e.branch)
        } else {
            old_tips[i - 1].clone()
        };
        match repo.git(&["rebase", "--onto", &e.parent, &upstream, &e.branch]) {
            Ok(_) => report.steps.push(RestackStep { branch: e.branch.clone(), moved: true }),
            Err(err) => {
                // Leave the rebase in progress: the conflict resolver picks it
                // up from here, and aborting would throw away the work.
                if repo.git(&["rev-parse", "--verify", "--quiet", "REBASE_HEAD"]).is_ok() {
                    report.conflicted = Some(e.branch.clone());
                    return Ok(report);
                }
                let _ = repo.checkout(&start);
                return Err(err);
            }
        }
    }
    if repo.current_branch() != start {
        repo.checkout(&start)?;
    }
    Ok(report)
}

fn rev_parse(repo: &Repo, rev: &str) -> Result<String> {
    repo.git(&["rev-parse", rev]).map(|o| o.trim().to_string())
}

/// Where `branch` left `parent`, preferring the reflog-aware answer.
///
/// `--fork-point` knows about a parent that has itself been rewritten; plain
/// `merge-base` is the fallback when the reflog has been pruned or the branch
/// arrived by fetch.
fn fork_point(repo: &Repo, parent: &str, branch: &str) -> String {
    repo.git(&["merge-base", "--fork-point", parent, branch])
        .ok()
        .map(|o| o.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| repo.git(&["merge-base", parent, branch]).ok().map(|o| o.trim().to_string()))
        .unwrap_or_else(|| parent.to_string())
}

// ---------------------------------------------------------------------------
// Merging out of the stack
// ---------------------------------------------------------------------------

/// Takes landed branches out of the stack, re-parenting what sat on them.
///
/// `merged` names the branches known to have landed — from the pull request's
/// state where that is known, since a squash merge is invisible to a patch-id
/// comparison. Their children inherit their parent, so the branch above a
/// merged one ends up based on the trunk and its PR retargets cleanly.
///
/// The branches themselves are left alone. Deleting them is the user's call.
pub fn drop_merged(repo: &Repo, stack: &Stack, merged: &[String]) -> Result<Vec<String>> {
    let mut dropped = Vec::new();
    for e in &stack.entries {
        if !merged.contains(&e.branch) {
            continue;
        }
        // Re-parent this entry's children onto what it was based on.
        for (child, parent) in parents(repo) {
            if parent == e.branch {
                set_parent(repo, &child, &e.parent)?;
            }
        }
        clear_parent(repo, &e.branch)?;
        clear_pr(repo, &e.branch)?;
        dropped.push(e.branch.clone());
    }
    Ok(dropped)
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// Pushes every branch in the stack, newest state wins.
///
/// Restacking rewrites history, so this force-pushes — with a lease, so a
/// branch someone else moved is refused rather than overwritten.
pub fn push_stack(repo: &Repo, stack: &Stack, auth: Option<&str>) -> Result<Vec<String>> {
    let mut pushed = Vec::new();
    for e in &stack.entries {
        repo.push_branch(&e.branch, true, auth)?;
        pushed.push(e.branch.clone());
    }
    Ok(pushed)
}

/// One row of the navigation block: a branch and the PR that carries it.
#[derive(Debug, Clone)]
pub struct NavRow {
    pub branch: String,
    pub pr: Option<u64>,
}

impl Stack {
    /// The rows [`nav_section`] renders, bottom-first.
    pub fn nav_rows(&self) -> Vec<NavRow> {
        self.entries
            .iter()
            .map(|e| NavRow { branch: e.branch.clone(), pr: e.pr })
            .collect()
    }
}

/// The stack map that goes in each pull request's body.
///
/// Top of the stack first, the way the branches sit — a reviewer reading a PR
/// wants to know what is underneath it. `current` is marked so the same block
/// can be posted to every PR in the stack and still say where you are.
pub fn nav_section(rows: &[NavRow], trunk: &str, current: &str) -> String {
    let mut out = String::from(NAV_START);
    out.push_str("\n\n**Stack** (top first)\n\n");
    for row in rows.iter().rev() {
        let label = match row.pr {
            Some(n) => format!("#{n}"),
            None => "(not submitted)".to_string(),
        };
        let here = if row.branch == current { "  ⬅ **this PR**" } else { "" };
        out.push_str(&format!("- {label} `{}`{here}\n", row.branch));
    }
    out.push_str(&format!("- `{trunk}`\n"));
    out.push_str("\n<sub>Managed by DevDock. Merge from the bottom up.</sub>\n");
    out.push_str(NAV_END);
    out
}

/// Puts `nav` into `body`, replacing any block already there.
///
/// Idempotent on purpose: every submit rewrites the block, and a PR body that
/// accumulated one stack map per push would be unreadable within a day.
pub fn with_nav(body: &str, nav: &str) -> String {
    if let (Some(start), Some(end)) = (body.find(NAV_START), body.find(NAV_END)) {
        if start < end {
            let mut out = String::with_capacity(body.len());
            out.push_str(&body[..start]);
            out.push_str(nav);
            out.push_str(&body[end + NAV_END.len()..]);
            return out;
        }
    }
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        return nav.to_string();
    }
    format!("{trimmed}\n\n{nav}")
}

/// A title and body for the pull request of one entry, from its commits.
///
/// The oldest commit names the change; the rest become the checklist a
/// reviewer reads before the diff. A PR opened from a stack is small by
/// construction, so this is usually all the description it needs.
pub fn draft_pr(entry: &StackEntry) -> (String, String) {
    let oldest = entry.commits.last();
    let title = oldest.map(|c| c.subject.clone()).unwrap_or_else(|| entry.branch.clone());
    let mut body = String::new();
    if let Some(c) = oldest {
        if !c.body.trim().is_empty() {
            body.push_str(c.body.trim());
            body.push_str("\n\n");
        }
    }
    if entry.commits.len() > 1 {
        body.push_str("**Commits**\n\n");
        for c in entry.commits.iter().rev() {
            body.push_str(&format!("- `{}` {}\n", c.short_sha, c.subject));
        }
    }
    (title, body)
}
