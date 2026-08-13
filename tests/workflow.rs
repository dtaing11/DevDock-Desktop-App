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
