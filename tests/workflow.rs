//! End-to-end tests of the git backend against throwaway repositories.

use git_manage::git::{FileStatus, Repo, RepoState, Resolution};
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
    repo.pull(None).unwrap();
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
