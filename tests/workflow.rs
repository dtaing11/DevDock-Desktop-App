//! End-to-end tests of the git backend against throwaway repositories.

use git_manage::git::{FileStatus, PullStrategy, Repo, RepoState, Resolution};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Runs a command in `dir`, panicking with stderr on failure.
fn sh(dir: &Path, cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Creates a work repo with a local bare "origin" remote inside a temp dir.
fn setup() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let bare = tmp.path().join("remote.git");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&bare).unwrap();
    sh(&bare, "git", &["init", "--bare"]);
    sh(&work, "git", &["init", "-b", "main"]);
    sh(&work, "git", &["config", "user.email", "test@test.io"]);
    sh(&work, "git", &["config", "user.name", "Tester"]);
    sh(&work, "git", &["remote", "add", "origin", bare.to_str().unwrap()]);
    let repo = Repo::open(&work).unwrap();
    (tmp, repo)
}

fn write(repo: &Repo, name: &str, content: &str) {
    fs::write(repo.path().join(name), content).unwrap();
}

fn read(repo: &Repo, name: &str) -> String {
    fs::read_to_string(repo.path().join(name)).unwrap()
}

fn commit_file(repo: &Repo, name: &str, content: &str, message: &str) {
    write(repo, name, content);
    repo.stage_all().unwrap();
    repo.commit(message, "", false).unwrap();
}

#[test]
fn status_stage_commit_log() {
    let (_tmp, repo) = setup();

    let status = repo.status().unwrap();
    assert!(status.files.is_empty());
    assert_eq!(status.state, RepoState::Clean);

    write(&repo, "a.txt", "hello\n");
    let status = repo.status().unwrap();
    assert_eq!(status.files.len(), 1);
    assert_eq!(status.files[0].work_status, Some(FileStatus::Untracked));

    let diff = repo.diff_file("a.txt", false).unwrap();
    assert!(diff.contains("+hello"), "diff was: {diff}");

    repo.stage(&["a.txt".into()]).unwrap();
    let sha = repo.commit("feat: add a", "body text", false).unwrap();
    assert_eq!(sha.len(), 40);
    assert!(repo.status().unwrap().files.is_empty());

    let log = repo.log(10, None).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].subject, "feat: add a");
    assert_eq!(log[0].body, "body text");
}

#[test]
fn push_pull_fetch_remotes() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "hello\n", "init");

    repo.push(true, None).unwrap();
    let status = repo.status().unwrap();
    assert!(status.has_upstream);
    assert_eq!(status.ahead, 0);

    let remotes = repo.remotes().unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");

    repo.fetch(None).unwrap();
    repo.pull(PullStrategy::FastForwardOnly, None).unwrap();
}

/// Rewinds `main` to `base` and refreshes tracking refs, leaving the repo in
/// the state of a machine that has not pulled since the remote moved on.
fn rewind_to(repo: &Repo, base: &str) {
    repo.git(&["reset", "--hard", base]).unwrap();
    repo.fetch(None).unwrap();
}

/// Pushes a merge of `feature` to origin, then rewinds local `main` — the
/// shape a pull request merged on GitHub leaves behind locally.
///
/// `main` gets a commit of its own before the merge so that merging
/// `feature` is a true three-way merge rather than a fast-forward. Without
/// that the duplicate would fast-forward away harmlessly and never
/// reproduce the divergence this guards against.
fn merge_on_remote_only(repo: &Repo) -> String {
    commit_file(repo, "a.txt", "base\n", "init");
    repo.push(true, None).unwrap();

    repo.create_branch("feature", true).unwrap();
    commit_file(repo, "b.txt", "feature\n", "feat: b");
    repo.push(true, None).unwrap();

    repo.checkout("main").unwrap();
    commit_file(repo, "c.txt", "main\n", "chore: c");
    repo.push(false, None).unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // GitHub writes its own merge message, which is what makes the remote's
    // merge a *different commit* from the local one despite identical parents
    // and tree. With the default message both merges would hash identically
    // inside a fast test and no divergence would appear at all.
    repo.git(&["merge", "--no-ff", "-m", "Merge pull request #1 from tester/feature", "feature"])
        .unwrap();
    let remote_merge = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    repo.push(false, None).unwrap();

    rewind_to(repo, &base);
    assert_ne!(base, remote_merge, "setup must leave a real merge on the remote");
    base
}

#[test]
fn merge_refuses_branch_already_in_history() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");
    repo.create_branch("feature", true).unwrap();
    commit_file(&repo, "b.txt", "feature\n", "feat: b");
    repo.checkout("main").unwrap();
    assert!(repo.merge("feature").ok);

    let outcome = repo.merge("feature");
    assert!(!outcome.ok, "re-merging an absorbed branch should be refused");
    assert!(
        outcome.message.contains("already merged here"),
        "unexpected message: {}",
        outcome.message
    );
}

/// Regression: merging a branch the remote already absorbed must be refused.
/// Running it builds a second merge commit carrying the same parents and tree
/// as the remote's; git compares commits by hash, so the two never reconcile
/// and the branch reports "ahead 1, behind 1" against its own upstream.
#[test]
fn merge_refuses_branch_the_remote_already_merged() {
    let (_tmp, repo) = setup();
    merge_on_remote_only(&repo);

    let outcome = repo.merge("feature");
    assert!(!outcome.ok, "duplicate merge should be refused");
    assert!(
        outcome.message.contains("already merged") && outcome.message.contains("remote"),
        "unexpected message: {}",
        outcome.message
    );

    let status = repo.status().unwrap();
    assert_eq!(status.ahead, 0, "a refused merge must not leave a local commit");
}

/// The refusal above is what keeps the branch fast-forwardable: after it,
/// a plain pull still lands cleanly instead of dead-ending on divergence.
#[test]
fn refused_duplicate_merge_leaves_branch_fast_forwardable() {
    let (_tmp, repo) = setup();
    merge_on_remote_only(&repo);
    let _ = repo.merge("feature");

    repo.pull(PullStrategy::FastForwardOnly, None).unwrap();
    let status = repo.status().unwrap();
    assert_eq!((status.ahead, status.behind), (0, 0));
}

/// Sets up a branch that has genuinely diverged from its upstream.
fn diverge(repo: &Repo) {
    commit_file(repo, "a.txt", "base\n", "init");
    repo.push(true, None).unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    commit_file(repo, "remote.txt", "remote\n", "remote work");
    repo.push(false, None).unwrap();

    repo.git(&["reset", "--hard", &base]).unwrap();
    commit_file(repo, "local.txt", "local\n", "local work");
    repo.fetch(None).unwrap();
}

/// Regression: a fast-forward pull that hits divergence must explain the two
/// ways out, never leak git's bare "Need to specify how to reconcile
/// divergent branches" fatal, which leaves the user stuck inside the app.
#[test]
fn fast_forward_pull_explains_divergence() {
    let (_tmp, repo) = setup();
    diverge(&repo);

    let err = repo.pull(PullStrategy::FastForwardOnly, None).unwrap_err().to_string();
    assert!(err.contains("Pull (rebase)"), "no way forward offered: {err}");
    assert!(
        !err.contains("Need to specify how to reconcile"),
        "raw git fatal leaked to the user: {err}"
    );

    let status = repo.status().unwrap();
    assert_eq!((status.ahead, status.behind), (1, 1), "failed pull must not alter history");
}

#[test]
fn rebase_pull_reconciles_diverged_branch() {
    let (_tmp, repo) = setup();
    diverge(&repo);

    repo.pull(PullStrategy::Rebase, None).unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status.behind, 0, "rebase pull should absorb the remote commits");
    assert_eq!(status.ahead, 1, "the local commit should be replayed on top");
    assert!(repo.path().join("remote.txt").exists());
    assert!(repo.path().join("local.txt").exists());
}

#[test]
fn merge_pull_reconciles_diverged_branch() {
    let (_tmp, repo) = setup();
    diverge(&repo);

    repo.pull(PullStrategy::Merge, None).unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status.behind, 0, "merge pull should absorb the remote commits");
    assert!(repo.path().join("remote.txt").exists());
    assert!(repo.path().join("local.txt").exists());
}

#[test]
fn branches_merge_rebase() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");

    // create/switch branches
    repo.create_branch("feature", true).unwrap();
    assert_eq!(repo.current_branch(), "feature");
    commit_file(&repo, "b.txt", "feature\n", "feat: b");

    // merge back
    repo.checkout("main").unwrap();
    let outcome = repo.merge("feature");
    assert!(outcome.ok, "merge failed: {}", outcome.message);
    assert!(repo.path().join("b.txt").exists());

    let branches = repo.branches().unwrap();
    assert_eq!(branches.current, "main");
    assert!(branches.local.iter().any(|b| b.name == "feature"));

    // rebase a divergent topic branch
    repo.create_branch("topic", true).unwrap();
    commit_file(&repo, "c.txt", "topic\n", "feat: c");
    repo.checkout("main").unwrap();
    commit_file(&repo, "d.txt", "main\n", "feat: d");
    repo.checkout("topic").unwrap();
    let outcome = repo.rebase("main");
    assert!(outcome.ok, "rebase failed: {}", outcome.message);
    let log = repo.log(10, None).unwrap();
    assert_eq!(log[0].subject, "feat: c");
    assert_eq!(log[1].subject, "feat: d");
}

#[test]
fn conflict_detection_and_resolution() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "conflict.txt", "base\n", "init");

    repo.create_branch("clash", true).unwrap();
    commit_file(&repo, "conflict.txt", "clash\n", "clash edit");
    repo.checkout("main").unwrap();
    commit_file(&repo, "conflict.txt", "main\n", "main edit");

    // merge hits a conflict
    let outcome = repo.merge("clash");
    assert!(!outcome.ok && outcome.conflict, "expected conflict: {}", outcome.message);
    assert_eq!(repo.state().unwrap(), RepoState::Merging);

    // three-way contents are exposed
    let conflicts = repo.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "conflict.txt");
    assert!(conflicts[0].ours.as_deref().unwrap().contains("main"));
    assert!(conflicts[0].theirs.as_deref().unwrap().contains("clash"));

    // resolve with "theirs" and finish the merge
    repo.resolve("conflict.txt", &Resolution::Theirs).unwrap();
    let outcome = repo.merge_continue();
    assert!(outcome.ok, "merge continue failed: {}", outcome.message);
    assert_eq!(repo.state().unwrap(), RepoState::Clean);
    assert_eq!(read(&repo, "conflict.txt"), "clash\n");

    // second conflict resolved manually
    repo.checkout("clash").unwrap();
    commit_file(&repo, "conflict.txt", "clash-2\n", "clash 2");
    repo.checkout("main").unwrap();
    commit_file(&repo, "conflict.txt", "main-2\n", "main 2");
    let outcome = repo.merge("clash");
    assert!(!outcome.ok && outcome.conflict);
    repo.resolve("conflict.txt", &Resolution::Manual("merged-by-hand\n".into())).unwrap();
    assert!(repo.merge_continue().ok);
    assert_eq!(read(&repo, "conflict.txt"), "merged-by-hand\n");
}

#[test]
fn discard_and_unstage() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "clean\n", "init");

    // discard restores tracked files and deletes untracked ones
    write(&repo, "a.txt", "dirty\n");
    write(&repo, "junk.txt", "x\n");
    repo.discard(&["a.txt".into(), "junk.txt".into()]).unwrap();
    assert_eq!(read(&repo, "a.txt"), "clean\n");
    assert!(!repo.path().join("junk.txt").exists());

    // stage_all / unstage_all round trip
    write(&repo, "a.txt", "staged\n");
    repo.stage_all().unwrap();
    assert!(repo.status().unwrap().files[0].staged);
    repo.unstage_all().unwrap();
    assert!(!repo.status().unwrap().files[0].staged);
}

/// Regression: discarding must clear *staged* work, not just unstaged edits.
/// `git checkout -- <path>` copies the index over the working tree, so
/// against a staged change it rewrites the file with the very content being
/// discarded: the edit survives in both places and the discard silently does
/// nothing. Restoring from `HEAD` is what actually clears it.
#[test]
fn discard_clears_staged_changes() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "clean\n", "init");

    write(&repo, "a.txt", "dirty\n");
    repo.stage_all().unwrap();
    assert!(repo.status().unwrap().files[0].staged, "setup should have staged the edit");

    repo.discard(&["a.txt".into()]).unwrap();
    assert_eq!(read(&repo, "a.txt"), "clean\n", "working tree kept the discarded edit");
    assert!(repo.status().unwrap().files.is_empty(), "index kept the discarded edit");
}

/// A file staged but never committed has no state in `HEAD` to restore to,
/// so discarding it removes it from the index and the working tree.
#[test]
fn discard_removes_staged_new_file() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "clean\n", "init");

    write(&repo, "added.txt", "new\n");
    repo.stage_all().unwrap();

    repo.discard(&["added.txt".into()]).unwrap();
    assert!(!repo.path().join("added.txt").exists(), "file left on disk");
    assert!(repo.status().unwrap().files.is_empty(), "index still lists the addition");
}

#[test]
fn discard_restores_staged_deletion() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "clean\n", "init");

    repo.git(&["rm", "--quiet", "a.txt"]).unwrap();
    repo.discard(&["a.txt".into()]).unwrap();

    assert_eq!(read(&repo, "a.txt"), "clean\n", "deleted file not restored");
    assert!(repo.status().unwrap().files.is_empty());
}

/// A rename is staged as a deletion of the original path plus an addition of
/// the new one, and the UI lists it under the new path alone. Discarding it
/// has to put the original back rather than just dropping the new name.
#[test]
fn discard_reverses_staged_rename() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "clean\n", "init");

    repo.git(&["mv", "a.txt", "b.txt"]).unwrap();
    let listed = repo.status().unwrap();
    assert_eq!(listed.files[0].path, "b.txt", "rename should be listed under its new path");

    repo.discard(&["b.txt".into()]).unwrap();
    assert_eq!(read(&repo, "a.txt"), "clean\n", "original path not restored");
    assert!(!repo.path().join("b.txt").exists(), "renamed path not removed");
    assert!(repo.status().unwrap().files.is_empty());
}

#[test]
fn diff_for_ai_prefers_staged_changes() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "one\n", "init");

    write(&repo, "a.txt", "two\n");
    repo.stage_all().unwrap();
    let diff = repo.diff_for_ai().unwrap();
    assert!(diff.contains("+two"), "ai diff: {diff}");

    // untracked-only repositories still produce a diff
    repo.commit("chore", "", false).unwrap();
    write(&repo, "fresh.txt", "brand new\n");
    let diff = repo.diff_for_ai().unwrap();
    assert!(diff.contains("+brand new"), "ai diff: {diff}");
}

#[test]
fn open_and_init() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fresh");
    let repo = Repo::init(&path).unwrap();
    assert_eq!(repo.name(), "fresh");
    assert!(Repo::open(repo.path()).is_ok());
    assert!(Repo::open("/nonexistent/nope").is_err());
}

#[test]
fn stash_save_list_pop_drop() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");

    write(&repo, "a.txt", "wip\n");
    write(&repo, "untracked.txt", "new\n");
    repo.stash_save("my work in progress").unwrap();
    assert_eq!(read(&repo, "a.txt"), "base\n");
    assert!(!repo.path().join("untracked.txt").exists());

    let stashes = repo.stash_list().unwrap();
    assert_eq!(stashes.len(), 1);
    assert!(stashes[0].message.contains("my work in progress"));

    repo.stash_pop(0).unwrap();
    assert_eq!(read(&repo, "a.txt"), "wip\n");
    assert!(repo.path().join("untracked.txt").exists());
    assert!(repo.stash_list().unwrap().is_empty());

    // drop path
    repo.stash_save("to drop").unwrap();
    repo.stash_drop(0).unwrap();
    assert!(repo.stash_list().unwrap().is_empty());
}

#[test]
fn undo_last_commit_keeps_changes_staged() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "one\n", "first");
    commit_file(&repo, "a.txt", "two\n", "second");
    assert_eq!(repo.log(10, None).unwrap().len(), 2);

    repo.undo_last_commit().unwrap();
    let log = repo.log(10, None).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].subject, "first");
    // Changes from the undone commit are staged.
    let status = repo.status().unwrap();
    assert_eq!(status.files.len(), 1);
    assert!(status.files[0].staged);
    assert_eq!(read(&repo, "a.txt"), "two\n");
}

#[test]
fn amend_commit_updates_message_and_content() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "one\n", "orig message");
    write(&repo, "a.txt", "one\nplus amended line\n");
    repo.stage_all().unwrap();
    repo.commit("amended message", "", true).unwrap();
    let log = repo.log(10, None).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].subject, "amended message");
    assert_eq!(read(&repo, "a.txt"), "one\nplus amended line\n");
}

#[test]
fn hunks_parse_and_stage_partially() {
    let (_tmp, repo) = setup();
    // File with two widely separated regions so edits become two hunks.
    let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    commit_file(&repo, "big.txt", &body, "init big");

    let mut lines: Vec<String> = body.lines().map(String::from).collect();
    lines[0] = "line 1 EDITED".into();
    lines[39] = "line 40 EDITED".into();
    write(&repo, "big.txt", &(lines.join("\n") + "\n"));

    let hunks = repo.hunks("big.txt").unwrap();
    assert_eq!(hunks.len(), 2, "expected 2 hunks: {hunks:?}");

    // Stage only the first hunk.
    repo.stage_hunk(&hunks[0]).unwrap();
    let staged = repo.diff_all(true).unwrap();
    let unstaged = repo.diff_all(false).unwrap();
    assert!(staged.contains("line 1 EDITED"));
    assert!(!staged.contains("line 40 EDITED"));
    assert!(unstaged.contains("line 40 EDITED"));
}

#[test]
fn commit_files_and_per_file_diff() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "a\n", "init");
    write(&repo, "b.txt", "b\n");
    write(&repo, "a.txt", "a changed\n");
    repo.stage_all().unwrap();
    let sha = repo.commit("touch two files", "", false).unwrap();

    let mut files = repo.commit_files(&sha).unwrap();
    files.sort_by(|x, y| x.path.cmp(&y.path));
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "a.txt");
    assert_eq!(files[0].status, FileStatus::Modified);
    assert_eq!(files[1].path, "b.txt");
    assert_eq!(files[1].status, FileStatus::Added);

    let diff = repo.diff_commit_file(&sha, "a.txt").unwrap();
    assert!(diff.contains("+a changed"));
    assert!(!diff.contains("+b"));
}

#[test]
fn tags_create_list_push() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "a\n", "init");
    repo.create_tag("v1.0.0", "first release").unwrap();
    repo.create_tag("v1.1.0", "").unwrap();
    let tags = repo.tags().unwrap();
    assert!(tags.contains(&"v1.0.0".to_string()));
    assert!(tags.contains(&"v1.1.0".to_string()));
    repo.push_tag("v1.0.0", None).unwrap();
}

#[test]
fn rename_branch_and_ignore() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "a\n", "init");

    repo.rename_branch("main", "trunk").unwrap();
    assert_eq!(repo.current_branch(), "trunk");

    repo.ignore("*.log").unwrap();
    repo.ignore("build/").unwrap();
    let gitignore = read(&repo, ".gitignore");
    assert!(gitignore.contains("*.log"));
    assert!(gitignore.contains("build/"));
    // Ignored files stay out of status.
    write(&repo, "noise.log", "x\n");
    assert!(!repo
        .status()
        .unwrap()
        .files
        .iter()
        .any(|f| f.path == "noise.log"));
}

#[test]
fn blame_reports_authors_per_line() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "first line\n", "one");
    write(&repo, "a.txt", "first line\nsecond line\n");
    repo.stage_all().unwrap();
    repo.commit("two", "", false).unwrap();

    let blame = repo.blame("a.txt").unwrap();
    assert_eq!(blame.len(), 2);
    assert_eq!(blame[0].line, "first line");
    assert_eq!(blame[1].line, "second line");
    assert_eq!(blame[0].author, "Tester");
    assert_ne!(blame[0].sha, blame[1].sha);
}

#[test]
fn new_branch_counts_only_unpushed_commits() {
    let (_tmp, repo) = setup();
    // Three commits pushed to origin on main.
    commit_file(&repo, "a.txt", "1\n", "one");
    commit_file(&repo, "a.txt", "2\n", "two");
    commit_file(&repo, "a.txt", "3\n", "three");
    repo.push(true, None).unwrap();

    // A fresh branch from pushed history has zero unpushed commits.
    repo.create_branch("feature", true).unwrap();
    let status = repo.status().unwrap();
    assert!(!status.has_upstream);
    assert_eq!(status.ahead, 0, "new branch off pushed history must show 0, not all history");

    // One new commit on the branch: exactly 1 unpushed.
    commit_file(&repo, "b.txt", "x\n", "branch work");
    assert_eq!(repo.status().unwrap().ahead, 1);

    // After publishing, back to 0.
    repo.push(true, None).unwrap();
    let status = repo.status().unwrap();
    assert!(status.has_upstream);
    assert_eq!(status.ahead, 0);
}

#[test]
fn force_push_with_lease_after_amend() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "v1\n", "original");
    repo.push(true, None).unwrap();

    // Amend the pushed commit; plain push now fails, force-with-lease works.
    write(&repo, "a.txt", "v2\n");
    repo.stage_all().unwrap();
    repo.commit("amended", "", true).unwrap();
    assert!(repo.push(false, None).is_err(), "plain push should be rejected");
    repo.force_push(None).unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status.ahead, 0);
    assert_eq!(status.behind, 0);
}

#[test]
fn revert_creates_inverse_commit() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "keep\n", "base");
    commit_file(&repo, "a.txt", "bad change\n", "bad commit");
    let bad_sha = repo.log(1, None).unwrap()[0].sha.clone();

    let outcome = repo.revert_commit(&bad_sha);
    assert!(outcome.ok, "revert failed: {}", outcome.message);
    assert_eq!(read(&repo, "a.txt"), "keep\n");
    let log = repo.log(5, None).unwrap();
    assert_eq!(log.len(), 3);
    assert!(log[0].subject.starts_with("Revert"));
}

#[test]
fn stage_individual_lines() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "base");
    // Two additions in the same hunk.
    write(&repo, "f.txt", "one\nADDED-A\ntwo\nADDED-B\nthree\n");
    let hunks = repo.hunks("f.txt").unwrap();
    assert_eq!(hunks.len(), 1);

    // Find body indices of the two '+' lines; select only the first.
    let plus_lines: Vec<usize> = hunks[0]
        .text
        .lines()
        .skip(1)
        .enumerate()
        .filter(|(_, l)| l.starts_with('+'))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(plus_lines.len(), 2);
    repo.stage_lines(&hunks[0], &plus_lines[..1]).unwrap();

    let staged = repo.diff_all(true).unwrap();
    assert!(staged.contains("ADDED-A"), "staged: {staged}");
    assert!(!staged.contains("ADDED-B"), "staged: {staged}");
    let unstaged = repo.diff_all(false).unwrap();
    assert!(unstaged.contains("ADDED-B"));
}

#[test]
fn rebase_progress_reports_counts() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "base.txt", "base\n", "base");
    // Branch with two commits that will conflict on rebase.
    repo.create_branch("topic", true).unwrap();
    commit_file(&repo, "conflict.txt", "topic-1\n", "topic 1");
    commit_file(&repo, "other.txt", "x\n", "topic 2");
    repo.checkout("main").unwrap();
    commit_file(&repo, "conflict.txt", "main-1\n", "main 1");
    repo.checkout("topic").unwrap();

    let outcome = repo.rebase("main");
    assert!(!outcome.ok && outcome.conflict);
    let (done, total) = repo.rebase_progress().expect("progress during rebase");
    assert_eq!(total, 2);
    assert!(done >= 1);
    repo.rebase_abort().unwrap();
    assert!(repo.rebase_progress().is_none());
}

#[test]
fn binary_detection() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "text.txt", "hello\n", "base");
    // Modify text file: not binary.
    write(&repo, "text.txt", "hello world\n");
    assert!(!repo.is_binary("text.txt"));
    // Commit a binary file then modify it.
    std::fs::write(repo.path().join("bin.dat"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
    repo.stage_all().unwrap();
    repo.commit("add binary", "", false).unwrap();
    std::fs::write(repo.path().join("bin.dat"), [7u8, 0, 3, 0, 255]).unwrap();
    assert!(repo.is_binary("bin.dat"));
}

#[test]
fn merge_into_lands_commits_on_target() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "base");

    // Work happens on a feature branch.
    repo.create_branch("feature", true).unwrap();
    commit_file(&repo, "feat.txt", "work\n", "feature work");

    // "Merge feature into main" from the feature branch.
    let outcome = repo.merge_into("main");
    assert!(outcome.ok, "{}", outcome.message);
    // We end up on main with the feature commit present.
    assert_eq!(repo.current_branch(), "main");
    assert!(repo.path().join("feat.txt").exists());
    let log = repo.log(5, None).unwrap();
    assert!(log.iter().any(|c| c.subject == "feature work"));

    // Degenerate case: same source and target.
    let outcome = repo.merge_into("main");
    assert!(!outcome.ok);
    assert!(outcome.message.contains("same branch"));
}

#[test]
fn pr_conflict_fix_flow() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "f.txt", "base\n", "base");
    repo.push(true, None).unwrap();

    // Feature branch diverges and conflicts with main.
    repo.create_branch("feature", true).unwrap();
    commit_file(&repo, "f.txt", "feature version\n", "feature edit");
    repo.push(true, None).unwrap();
    repo.checkout("main").unwrap();
    commit_file(&repo, "f.txt", "main version\n", "main edit");
    repo.push(false, None).unwrap();

    // Simulate the app's "Fix conflicts" on the PR (head=feature, base=main).
    let outcome = repo.start_pr_conflict_fix("feature", "main");
    assert!(!outcome.ok && outcome.conflict, "expected conflict: {}", outcome.message);
    assert_eq!(repo.current_branch(), "feature");
    assert_eq!(repo.state().unwrap(), RepoState::Merging);

    // Resolve and complete, exactly as the resolver UI does.
    repo.resolve("f.txt", &Resolution::Manual("merged result\n".into())).unwrap();
    assert!(repo.merge_continue().ok);
    assert_eq!(repo.state().unwrap(), RepoState::Clean);
    // Pushing the head branch would now clear the PR's conflict state.
    repo.push(false, None).unwrap();
    assert_eq!(read(&repo, "f.txt"), "merged result\n");
}

#[test]
fn patch_line_numbering() {
    use git_manage::github::parse_patch_lines;
    let patch = "@@ -1,3 +1,4 @@\n context\n-removed\n+added one\n+added two\n context2";
    let lines = parse_patch_lines(patch);
    assert_eq!(lines[0].old_line, None); // hunk header
    assert_eq!(lines[1].old_line, Some(1)); // context
    assert_eq!(lines[1].new_line, Some(1));
    assert_eq!(lines[2].old_line, Some(2)); // removed: old side only
    assert_eq!(lines[2].new_line, None);
    assert_eq!(lines[3].new_line, Some(2)); // added: new side only
    assert_eq!(lines[3].old_line, None);
    assert_eq!(lines[4].new_line, Some(3));
    assert_eq!(lines[5].old_line, Some(3)); // trailing context advances both
    assert_eq!(lines[5].new_line, Some(4));
}

// ---------------------------------------------------------------------------
// AI review gate
// ---------------------------------------------------------------------------

/// The reviewer must see the commits a push would publish, not the working
/// tree: reviewing uncommitted edits would review code that isn't going out.
#[test]
fn review_diff_covers_outgoing_commits_not_working_tree() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");
    repo.push(true, None).unwrap();

    commit_file(&repo, "a.txt", "published\n", "feat: committed work");
    write(&repo, "a.txt", "uncommitted scratch\n");

    let diff = repo.diff_for_review(None).unwrap();
    assert!(diff.contains("+published"), "outgoing commit missing: {diff}");
    assert!(
        !diff.contains("uncommitted scratch"),
        "working-tree edit must not be reviewed: {diff}"
    );
}

/// With no upstream there is no `@{upstream}` to diff against, so the range
/// is derived from the oldest commit no remote has.
#[test]
fn review_diff_works_without_an_upstream() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");
    repo.push(true, None).unwrap();

    repo.create_branch("feature", true).unwrap();
    commit_file(&repo, "b.txt", "feature work\n", "feat: b");

    let diff = repo.diff_for_review(None).unwrap();
    assert!(diff.contains("+feature work"), "unpushed commit missing: {diff}");
}

/// A pull request is reviewed against its target branch.
#[test]
fn review_diff_against_a_base_branch() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");
    repo.create_branch("feature", true).unwrap();
    commit_file(&repo, "b.txt", "pr work\n", "feat: b");

    let diff = repo.diff_for_review(Some("main")).unwrap();
    assert!(diff.contains("+pr work"), "PR diff missing the change: {diff}");
}

/// A branch with nothing outgoing yields an empty diff rather than an error,
/// so the gate reports "nothing to review" instead of failing.
#[test]
fn review_diff_is_empty_when_nothing_is_outgoing() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "base\n", "init");
    repo.push(true, None).unwrap();

    assert!(repo.diff_for_review(None).unwrap().trim().is_empty());
}

/// Review settings live beside the jobs in the same config file, and are
/// off unless the repository opts in.
#[test]
fn review_config_parses_from_the_ci_file() {
    use git_manage::review::Severity;

    let (_tmp, repo) = setup();
    fs::write(
        repo.path().join(git_manage::local_ci::CONFIG_FILE),
        r#"
[[job]]
name = "tests"
commands = ["true"]

[review]
run = true
fail_on = "medium"
instructions = "Watch the UI thread."
"#,
    )
    .unwrap();

    let config = git_manage::local_ci::load_config(repo.path()).unwrap().unwrap();
    assert_eq!(config.jobs.len(), 1);
    assert!(config.review.run);
    assert_eq!(config.review.fail_on, Severity::Medium);
    assert_eq!(config.review.instructions.as_deref(), Some("Watch the UI thread."));
    // Unset fields keep their defaults.
    assert!(config.review.block_on_failure);
    assert_eq!(config.review.max_diff_bytes, 24_000);
}

/// A config with no `[review]` section leaves the reviewer off, so adding
/// the feature cannot change behaviour for existing repositories.
#[test]
fn review_is_off_when_the_config_omits_it() {
    let (_tmp, repo) = setup();
    fs::write(
        repo.path().join(git_manage::local_ci::CONFIG_FILE),
        "[[job]]\nname = \"tests\"\ncommands = [\"true\"]\n",
    )
    .unwrap();

    let config = git_manage::local_ci::load_config(repo.path()).unwrap().unwrap();
    assert!(!config.review.run, "review must be opt-in");
}

/// Keeps the guide honest: every `[review]`/`[on_push]` example in
/// `docs/local-ci.md` must deserialize into the real config types. A
/// documented field that no longer exists is a silent lie otherwise.
#[test]
fn documented_config_examples_match_the_real_schema() {
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/local-ci.md");
    let text = fs::read_to_string(&doc).expect("docs/local-ci.md should exist");

    // Pull out fenced ```toml blocks.
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (&mut current, line.trim()) {
            (None, "```toml") => current = Some(String::new()),
            (Some(_), "```") => blocks.push(current.take().unwrap()),
            (Some(buf), _) => {
                buf.push_str(line);
                buf.push('\n');
            }
            _ => {}
        }
    }
    assert!(blocks.len() >= 10, "expected the guide's toml examples, found {}", blocks.len());

    let mut checked = 0;
    for block in &blocks {
        if !block.contains("[review]") && !block.contains("[on_push]") {
            continue;
        }
        // `deny_unknown_fields` is not set on the config types, so compare
        // against a strict parse of the same text to catch stale field names.
        let parsed: git_manage::local_ci::Config = toml::from_str(block)
            .unwrap_or_else(|e| panic!("documented example does not parse:\n{block}\n{e}"));
        let raw: toml::Table = toml::from_str(block).unwrap();
        if let Some(toml::Value::Table(review)) = raw.get("review") {
            let round_trip = toml::Value::try_from(&parsed.review).unwrap();
            for key in review.keys() {
                assert!(
                    round_trip.get(key).is_some(),
                    "docs document `[review] {key}`, which the ReviewConfig struct \
                     does not have"
                );
            }
            checked += 1;
        }
        if let Some(toml::Value::Table(on_push)) = raw.get("on_push") {
            let round_trip = toml::Value::try_from(&parsed.on_push).unwrap();
            for key in on_push.keys() {
                assert!(
                    round_trip.get(key).is_some(),
                    "docs document `[on_push] {key}`, which the OnPush struct \
                     does not have"
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 3, "expected several gate examples in the guide, checked {checked}");
}

/// Regression: a tracked file inside a gitignored directory must not be able
/// to block staging everything else.
///
/// `git add` refuses any path under an ignored directory — even a tracked
/// one — and fails the *whole* invocation, so batching every path into one
/// `git add` meant a single such file broke every commit. (This is the state
/// a repo lands in when a build directory is committed and `.gitignore` gains
/// the directory afterwards.)
#[test]
fn staging_survives_a_tracked_file_in_an_ignored_directory() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "one\n", "init");

    // Commit a build artifact, then start ignoring the directory.
    fs::create_dir_all(repo.path().join("dist")).unwrap();
    write(&repo, "dist/app.bin", "v1\n");
    repo.git(&["add", "--force", "--", "dist/app.bin"]).unwrap();
    repo.commit("chore: add build output", "", false).unwrap();
    write(&repo, ".gitignore", "/dist\n");
    commit_file(&repo, ".gitignore", "/dist\n", "chore: ignore dist");

    // Now change both the ignored-but-tracked artifact and a normal file.
    write(&repo, "dist/app.bin", "v2\n");
    write(&repo, "a.txt", "two\n");

    // Batching these into one `git add` fails outright; staging must cope.
    let skipped = repo
        .stage(&["a.txt".into(), "dist/app.bin".into()])
        .expect("staging must not fail because of the ignored directory");
    assert!(skipped.is_empty(), "tracked paths should be force-added, not skipped: {skipped:?}");

    let status = repo.status().unwrap();
    let staged: Vec<&str> =
        status.files.iter().filter(|f| f.staged).map(|f| f.path.as_str()).collect();
    assert!(staged.contains(&"a.txt"), "normal file was not staged: {staged:?}");
    assert!(staged.contains(&"dist/app.bin"), "tracked artifact was not staged: {staged:?}");

    // And the commit actually goes through.
    repo.commit("chore: both", "", false).unwrap();
    assert!(repo.status().unwrap().files.is_empty());
}

/// A path that is ignored *and* untracked is reported, not forced into the
/// repository, and does not stop the rest from staging.
#[test]
fn staging_skips_but_reports_an_ignored_untracked_path() {
    let (_tmp, repo) = setup();
    commit_file(&repo, ".gitignore", "/dist\n", "chore: ignore dist");
    commit_file(&repo, "a.txt", "one\n", "init");

    fs::create_dir_all(repo.path().join("dist")).unwrap();
    write(&repo, "dist/app.bin", "fresh\n");
    write(&repo, "a.txt", "two\n");

    let skipped = repo.stage(&["a.txt".into(), "dist/app.bin".into()]).unwrap();
    assert_eq!(skipped, vec!["dist/app.bin".to_string()], "should report what it left out");

    let status = repo.status().unwrap();
    assert!(status.files.iter().any(|f| f.path == "a.txt" && f.staged));
    // The ignored file must not have been sneaked in.
    assert!(
        !repo.git(&["ls-files", "--", "dist/app.bin"]).unwrap().contains("app.bin"),
        "an ignored, untracked file must never be force-added"
    );
}

/// When every requested path is unstageable that is a real error, not a
/// silent no-op commit.
#[test]
fn staging_errors_when_nothing_could_be_staged() {
    let (_tmp, repo) = setup();
    commit_file(&repo, ".gitignore", "/dist\n", "chore: ignore dist");

    fs::create_dir_all(repo.path().join("dist")).unwrap();
    write(&repo, "dist/app.bin", "fresh\n");

    let err = repo.stage(&["dist/app.bin".into()]).unwrap_err().to_string();
    assert!(err.contains("Could not stage"), "unexpected error: {err}");
}

// ---------------------------------------------------------------------------
// Nested CI configs (monorepo)
// ---------------------------------------------------------------------------

fn write_ci(repo: &Repo, dir: &str, body: &str) {
    let target = if dir.is_empty() { repo.path().to_path_buf() } else { repo.path().join(dir) };
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join(git_manage::local_ci::CONFIG_FILE), body).unwrap();
}

/// Every config in the tree contributes jobs, and each job is tagged with the
/// directory it came from.
#[test]
fn nested_configs_all_contribute_jobs() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");

    write_ci(&repo, "", "[[job]]\nname = \"root\"\ncommands = [\"true\"]\n");
    write_ci(&repo, "packages/api", "[[job]]\nname = \"tests\"\ncommands = [\"true\"]\n");
    write_ci(&repo, "packages/web", "[[job]]\nname = \"tests\"\ncommands = [\"true\"]\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    let names: Vec<String> = loaded.config.jobs.iter().map(|j| j.display_name()).collect();

    assert_eq!(loaded.config.jobs.len(), 3, "got {names:?}");
    // Root first, then nested in path order.
    assert_eq!(names[0], "root");
    // Same job name in two packages must not be ambiguous.
    assert!(names.contains(&"packages/api: tests".to_string()), "{names:?}");
    assert!(names.contains(&"packages/web: tests".to_string()), "{names:?}");
    assert_eq!(loaded.sources.len(), 3);
}

/// A nested job's commands run in that directory, not the repo root — a
/// package's `cargo test` has to run in the package.
#[test]
fn a_nested_job_runs_in_its_own_directory() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "packages/api", "[[job]]\nname = \"where\"\ncommands = [\"pwd\"]\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    let job = loaded.config.jobs.first().expect("expected the nested job");
    assert_eq!(job.dir, "packages/api");

    let result = git_manage::local_ci::run_job(repo.path(), job);
    assert!(result.ok, "job failed: {}", result.output);
    assert!(
        result.output.trim_end().ends_with("packages/api"),
        "ran in the wrong directory: {}",
        result.output
    );
    assert_eq!(result.name, "packages/api: where");
}

/// Discovery goes through git, so gitignored paths are never searched — a
/// config left in `target/` or `node_modules/` must not add jobs.
#[test]
fn ignored_directories_are_not_searched() {
    let (_tmp, repo) = setup();
    write_ci(&repo, "", "[[job]]\nname = \"root\"\ncommands = [\"true\"]\n");
    commit_file(&repo, ".gitignore", "/target\n/node_modules\n", "chore: ignore");

    write_ci(&repo, "target/leftover", "[[job]]\nname = \"stale\"\ncommands = [\"false\"]\n");
    write_ci(&repo, "node_modules/pkg", "[[job]]\nname = \"vendored\"\ncommands = [\"false\"]\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    let names: Vec<String> = loaded.config.jobs.iter().map(|j| j.display_name()).collect();
    assert_eq!(names, vec!["root"], "ignored paths leaked in: {names:?}");
}

/// An uncommitted config still counts — you should not have to commit before
/// the checks you just wrote will run.
#[test]
fn an_uncommitted_nested_config_is_found() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "svc", "[[job]]\nname = \"fresh\"\ncommands = [\"true\"]\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    assert_eq!(
        loaded.config.jobs.iter().map(|j| j.display_name()).collect::<Vec<_>>(),
        vec!["svc: fresh"]
    );
}

/// Gates are repository-wide: only the root config's are honoured, and a
/// nested one is reported rather than silently dropped.
#[test]
fn gates_come_from_the_root_and_nested_ones_are_reported() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(
        &repo,
        "",
        "[on_push]\nrun = true\n\n[review]\nrun = true\nfail_on = \"medium\"\n",
    );
    write_ci(
        &repo,
        "svc",
        "[[job]]\nname = \"t\"\ncommands = [\"true\"]\n\n[on_push]\nrun = false\n\n[review]\nrun = true\n",
    );

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    assert!(loaded.config.on_push.run, "root [on_push] must win");
    assert_eq!(loaded.config.review.fail_on, git_manage::review::Severity::Medium);
    assert_eq!(loaded.ignored_gates, vec!["svc".to_string()], "must report what it ignored");
}

/// A repo with no root config still runs the nested ones.
#[test]
fn nested_configs_work_without_a_root_config() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "svc", "[[job]]\nname = \"t\"\ncommands = [\"true\"]\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    assert_eq!(loaded.config.jobs.len(), 1);
    assert!(!loaded.config.on_push.run, "no root config means default gates");
}

/// One unparseable nested config must not take down the rest.
#[test]
fn a_broken_nested_config_is_skipped() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "", "[[job]]\nname = \"root\"\ncommands = [\"true\"]\n");
    write_ci(&repo, "good", "[[job]]\nname = \"ok\"\ncommands = [\"true\"]\n");
    write_ci(&repo, "bad", "this is not = valid toml [[[\n");

    let loaded = git_manage::local_ci::discover_configs(repo.path()).unwrap();
    let names: Vec<String> = loaded.config.jobs.iter().map(|j| j.display_name()).collect();
    assert!(names.contains(&"root".to_string()), "{names:?}");
    assert!(names.contains(&"good: ok".to_string()), "{names:?}");
    assert_eq!(names.len(), 2, "the broken config should be skipped: {names:?}");
}

/// Regression: `devdock ci` and the pre-push hook must run the same jobs as
/// the app. Loading only the root config meant a monorepo's package checks
/// were skipped on the CLI — so the hook that gates pushes reported success
/// while the app showed failures.
#[test]
fn the_cli_runner_sees_nested_configs() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "", "[[job]]\nname = \"root\"\ncommands = [\"true\"]\n");
    write_ci(&repo, "packages/api", "[[job]]\nname = \"t\"\ncommands = [\"false\"]\n");

    // The nested job fails, so the whole run must fail.
    let ok = git_manage::local_ci::run_all_cli(repo.path()).unwrap();
    assert!(!ok, "a failing nested job must fail the CLI run");
}

/// With only a root config the CLI behaves exactly as before.
#[test]
fn the_cli_runner_still_handles_a_single_config() {
    let (_tmp, repo) = setup();
    commit_file(&repo, "a.txt", "x\n", "init");
    write_ci(&repo, "", "[[job]]\nname = \"root\"\ncommands = [\"true\"]\n");
    assert!(git_manage::local_ci::run_all_cli(repo.path()).unwrap());
}
