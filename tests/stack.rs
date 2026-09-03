//! Stacked pull requests: the parent chain, restacking, and the PR body block.

use git_manage::git::Repo;
use git_manage::stack::{self, NavRow};
use std::fs;
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
    commit(&repo, "root.txt", "root\n", "chore: root");
    (tmp, repo)
}

fn read(repo: &Repo, name: &str) -> String {
    fs::read_to_string(repo.path().join(name)).unwrap()
}

fn commit(repo: &Repo, name: &str, content: &str, message: &str) {
    fs::write(repo.path().join(name), content).unwrap();
    repo.stage_all().unwrap();
    repo.commit(message, "", false).unwrap();
}

/// Branch off the current HEAD and record the parent, the way the app does.
fn branch_on(repo: &Repo, name: &str, parent: &str) {
    repo.checkout(parent).unwrap();
    repo.create_branch(name, true).unwrap();
    stack::set_parent(repo, name, parent).unwrap();
}

/// Subjects of `main..branch`, oldest first.
fn subjects(repo: &Repo, branch: &str) -> Vec<String> {
    let range = format!("main..{branch}");
    let mut v: Vec<String> =
        repo.log(50, Some(&range)).unwrap().iter().map(|c| c.subject.clone()).collect();
    v.reverse();
    v
}

/// main → a(A1) → b(B1), checked out on `b`.
fn two_high(repo: &Repo) {
    branch_on(repo, "a", "main");
    commit(repo, "a.txt", "a1\n", "feat: A1");
    branch_on(repo, "b", "a");
    commit(repo, "b.txt", "b1\n", "feat: B1");
}

#[test]
fn parent_links_round_trip() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");

    assert_eq!(stack::parent_of(&repo, "a").as_deref(), Some("main"));
    assert_eq!(stack::parents(&repo).get("a").map(String::as_str), Some("main"));

    stack::clear_parent(&repo, "a").unwrap();
    assert_eq!(stack::parent_of(&repo, "a"), None);
    // Clearing an already-untracked branch is not an error.
    stack::clear_parent(&repo, "a").unwrap();
}

#[test]
fn parent_links_survive_slashes_in_branch_names() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "feat/one", "main");
    branch_on(&repo, "feat/two", "feat/one");

    let stack = stack::stack_for(&repo, "feat/two").unwrap();
    let names: Vec<&str> = stack.entries.iter().map(|e| e.branch.as_str()).collect();
    assert_eq!(names, ["feat/one", "feat/two"]);
}

#[test]
fn set_parent_refuses_a_loop() {
    let (_tmp, repo) = setup();
    two_high(&repo);

    assert!(stack::set_parent(&repo, "a", "b").is_err(), "a on b closes a loop");
    assert!(stack::set_parent(&repo, "a", "a").is_err(), "a on itself");
    // The refusal left the existing link alone.
    assert_eq!(stack::parent_of(&repo, "a").as_deref(), Some("main"));
}

#[test]
fn stack_for_builds_the_chain_from_any_member() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    branch_on(&repo, "c", "b");
    commit(&repo, "c.txt", "c1\n", "feat: C1");
    repo.checkout("b").unwrap();

    // Asked about the middle branch, the whole chain comes back.
    let stack = stack::stack_for(&repo, "b").unwrap();
    assert_eq!(stack.trunk, "main");
    let names: Vec<&str> = stack.entries.iter().map(|e| e.branch.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"], "bottom first");
    assert_eq!(stack.current, Some(1), "b is checked out");
    assert_eq!(stack.tip(), "c");

    let parents: Vec<&str> = stack.entries.iter().map(|e| e.parent.as_str()).collect();
    assert_eq!(parents, ["main", "a", "b"]);

    // Each entry carries only its own commits.
    for e in &stack.entries {
        assert_eq!(e.commits.len(), 1, "{} carried {:?}", e.branch, e.commits);
    }
    assert_eq!(stack.entries[0].summary(), "feat: A1");
    assert!(!stack.entries.iter().any(|e| e.needs_restack), "nothing is stale yet");
}

#[test]
fn an_untracked_branch_is_a_stack_of_one() {
    let (_tmp, repo) = setup();
    repo.create_branch("solo", true).unwrap();
    commit(&repo, "s.txt", "s\n", "feat: solo");

    let stack = stack::stack_for(&repo, "solo").unwrap();
    assert_eq!(stack.entries.len(), 1);
    assert_eq!(stack.entries[0].parent, "main");
    assert_eq!(stack.tip(), "solo");

    // The trunk itself is not an entry: it is what entries are based on.
    repo.checkout("main").unwrap();
    assert!(stack::stack_for(&repo, "main").unwrap().is_empty());
}

#[test]
fn stack_for_reports_a_fork_instead_of_guessing() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");
    commit(&repo, "a.txt", "a1\n", "feat: A1");
    branch_on(&repo, "left", "a");
    branch_on(&repo, "right", "a");

    let stack = stack::stack_for(&repo, "a").unwrap();
    assert_eq!(stack.entries.len(), 1, "the chain stops where it forks");
    assert_eq!(stack.forks, ["left", "right"]);
}

#[test]
fn restack_replays_only_each_branch_own_commits() {
    let (_tmp, repo) = setup();
    two_high(&repo);

    // A new commit lands on the bottom branch; everything above is now stale.
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");

    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(stack.entry("b").unwrap().needs_restack);
    assert!(!stack.entry("a").unwrap().needs_restack);

    let report = stack::restack(&repo, &stack).unwrap();
    assert_eq!(report.conflicted, None);
    assert_eq!(report.moved(), ["b"], "only the stale branch moved");

    // B1 sits on top of both of a's commits, and A1 appears exactly once.
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: A2", "feat: B1"]);
    assert_eq!(repo.current_branch(), "a", "the checked-out branch is restored");
}

#[test]
fn restack_rebuilds_the_whole_chain_when_the_trunk_moves() {
    let (_tmp, repo) = setup();
    two_high(&repo);

    repo.checkout("main").unwrap();
    commit(&repo, "m.txt", "m\n", "feat: M1");
    repo.checkout("b").unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(stack.entry("a").unwrap().needs_restack, "a is behind main");

    let report = stack::restack(&repo, &stack).unwrap();
    assert_eq!(report.conflicted, None);
    assert_eq!(report.moved(), ["a", "b"]);

    // Both branches rebuilt on the new main, each commit still exactly once.
    assert_eq!(subjects(&repo, "a"), ["feat: A1"]);
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: B1"]);
    assert_eq!(repo.current_branch(), "b");
}

#[test]
fn restack_is_a_noop_when_the_stack_is_in_order() {
    let (_tmp, repo) = setup();
    two_high(&repo);

    let stack = stack::stack_for(&repo, "b").unwrap();
    let before = repo.git(&["rev-parse", "b"]).unwrap();
    let report = stack::restack(&repo, &stack).unwrap();

    assert!(report.moved().is_empty());
    assert_eq!(report.message(), "Stack is already in order.");
    assert_eq!(repo.git(&["rev-parse", "b"]).unwrap(), before, "no rewrite");
}

#[test]
fn restack_refuses_a_dirty_working_tree() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");
    fs::write(repo.path().join("dirty.txt"), "uncommitted\n").unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    let err = stack::restack(&repo, &stack).unwrap_err().to_string();
    assert!(err.contains("uncommitted"), "unhelpful message: {err}");
    // Nothing was rewritten on the way to refusing.
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: B1"]);
}

#[test]
fn restack_leaves_a_conflict_for_the_resolver() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");
    commit(&repo, "shared.txt", "from a\n", "feat: A1");
    branch_on(&repo, "b", "a");
    commit(&repo, "shared.txt", "from b\n", "feat: B1");

    // Rewrite a's commit so b's change no longer applies cleanly.
    repo.checkout("a").unwrap();
    fs::write(repo.path().join("shared.txt"), "from a, revised\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("feat: A1 revised", "", true).unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    let report = stack::restack(&repo, &stack).unwrap();

    assert_eq!(report.conflicted.as_deref(), Some("b"));
    assert!(report.message().contains("conflicts"), "{}", report.message());
    // The rebase is still in progress, so the conflict resolver can finish it.
    assert!(!repo.conflicts().unwrap().is_empty(), "conflicts are on disk");
    repo.rebase_abort().unwrap();
}

#[test]
fn a_landed_branch_is_seen_as_merged() {
    let (_tmp, repo) = setup();
    two_high(&repo);

    // Not merged yet.
    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(!stack.entry("a").unwrap().merged);

    // Land `a` on main the way a rebase-merge would: same change, new sha.
    repo.checkout("main").unwrap();
    let sha = repo.git(&["rev-parse", "a"]).unwrap();
    sh(repo.path(), "git", &["cherry-pick", sha.trim()]);

    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(stack.entry("a").unwrap().merged, "cherry compares patches, not shas");
    assert!(!stack.entry("b").unwrap().merged, "b has not landed");
}

#[test]
fn a_fast_forwarded_branch_is_seen_as_merged() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("main").unwrap();
    sh(repo.path(), "git", &["merge", "--ff-only", "a"]);

    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(stack.entry("a").unwrap().merged);
}

#[test]
fn dropping_a_merged_branch_reparents_what_sat_on_it() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    branch_on(&repo, "c", "b");
    commit(&repo, "c.txt", "c1\n", "feat: C1");

    let stack = stack::stack_for(&repo, "c").unwrap();
    let dropped = stack::drop_merged(&repo, &stack, &["a".to_string()]).unwrap();

    assert_eq!(dropped, ["a"]);
    assert_eq!(stack::parent_of(&repo, "b").as_deref(), Some("main"), "b moved down");
    assert_eq!(stack::parent_of(&repo, "a"), None, "a left the stack");
    let names: Vec<String> =
        stack::stack_for(&repo, "c").unwrap().entries.iter().map(|e| e.branch.clone()).collect();
    assert_eq!(names, ["b", "c"]);
}

#[test]
fn push_stack_publishes_every_branch_without_checking_them_out() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("a").unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    let pushed = stack::push_stack(&repo, &stack, None).unwrap();

    assert_eq!(pushed, ["a", "b"]);
    assert_eq!(repo.current_branch(), "a", "pushing did not move HEAD");
    let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
    assert!(remote.contains("refs/heads/a"), "{remote}");
    assert!(remote.contains("refs/heads/b"), "{remote}");
}

#[test]
fn push_stack_updates_a_restacked_branch() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    let stack = stack::stack_for(&repo, "b").unwrap();
    stack::push_stack(&repo, &stack, None).unwrap();

    // Rewrite the bottom branch, restack, and publish again: the force-with-lease
    // push is what makes the second publish land at all.
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");
    let stack = stack::stack_for(&repo, "b").unwrap();
    stack::restack(&repo, &stack).unwrap();
    let stack = stack::stack_for(&repo, "b").unwrap();
    stack::push_stack(&repo, &stack, None).unwrap();

    let local = repo.git(&["rev-parse", "b"]).unwrap();
    let remote = repo.git(&["rev-parse", "refs/remotes/origin/b"]).unwrap();
    assert_eq!(local, remote);
}

#[test]
fn the_navigation_block_marks_the_current_pr() {
    let rows = vec![
        NavRow { branch: "a".into(), pr: Some(10) },
        NavRow { branch: "b".into(), pr: Some(11) },
        NavRow { branch: "c".into(), pr: None },
    ];
    let nav = stack::nav_section(&rows, "main", "b");

    let body: Vec<&str> = nav.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(
        body,
        [
            "- (not submitted) `c`",
            "- #11 `b`  ⬅ **this PR**",
            "- #10 `a`",
            "- `main`",
        ],
        "top of the stack first, trunk at the bottom"
    );
}

#[test]
fn the_navigation_block_replaces_itself_rather_than_piling_up() {
    let rows = vec![NavRow { branch: "a".into(), pr: Some(1) }];
    let first = stack::nav_section(&rows, "main", "a");
    let body = stack::with_nav("Real description.", &first);
    assert!(body.starts_with("Real description."));

    let rows = vec![
        NavRow { branch: "a".into(), pr: Some(1) },
        NavRow { branch: "b".into(), pr: Some(2) },
    ];
    let second = stack::nav_section(&rows, "main", "a");
    let updated = stack::with_nav(&body, &second);

    assert_eq!(updated.matches(stack::NAV_START).count(), 1, "one block, not two");
    assert!(updated.contains("`b`"), "the new stack is in it");
    assert!(updated.starts_with("Real description."), "the human text survived");

    // Idempotent: rewriting with the same block changes nothing.
    assert_eq!(stack::with_nav(&updated, &second), updated);
}

#[test]
fn the_navigation_block_handles_an_empty_body() {
    let rows = vec![NavRow { branch: "a".into(), pr: None }];
    let nav = stack::nav_section(&rows, "main", "a");
    assert_eq!(stack::with_nav("", &nav), nav);
    assert_eq!(stack::with_nav("   \n\n", &nav), nav);
}

#[test]
fn a_pull_request_drafts_itself_from_the_branch_commits() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("feat: the headline", "why it was needed", false).unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: follow-up");

    let stack = stack::stack_for(&repo, "a").unwrap();
    let (title, body) = stack::draft_pr(stack.entry("a").unwrap());

    assert_eq!(title, "feat: the headline", "the oldest commit names the branch");
    assert!(body.starts_with("why it was needed"), "{body}");
    assert!(body.contains("- `"), "the commit list is there: {body}");
    let order: Vec<&str> = body.lines().filter(|l| l.starts_with("- `")).collect();
    assert!(order[0].contains("the headline"), "oldest first: {order:?}");
}

#[test]
fn a_detached_head_has_no_stack() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    let sha = repo.git(&["rev-parse", "a"]).unwrap();
    sh(repo.path(), "git", &["checkout", "--detach", sha.trim()]);

    // `current_branch` describes a detached HEAD in words; there is no branch
    // to stack, and inventing an entry for it would offer to rebase a phrase.
    let branch = repo.current_branch();
    let stack = stack::stack_for(&repo, &branch).unwrap();
    assert!(stack.is_empty(), "{:?}", stack.entries);
    assert_eq!(stack.tip(), "main");
}

#[test]
fn the_trunk_itself_is_never_an_entry() {
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("main").unwrap();

    let stack = stack::stack_for(&repo, "main").unwrap();
    assert!(stack.is_empty());
    assert_eq!(stack.trunk, "main");
}

#[test]
fn a_squash_merged_branch_leaves_without_replaying_its_commits() {
    let (_tmp, repo) = setup();
    // Two commits on `a`, so a squash of it is one commit whose patch matches
    // neither — the case where nothing local can tell it landed, and where a
    // rebase from the trunk would apply its changes a second time.
    branch_on(&repo, "a", "main");
    commit(&repo, "a1.txt", "a1\n", "feat: A1");
    commit(&repo, "a2.txt", "a2\n", "feat: A2");
    branch_on(&repo, "b", "a");
    commit(&repo, "b.txt", "b1\n", "feat: B1");

    // Squash-merge `a` the way GitHub's "Squash and merge" does.
    repo.checkout("main").unwrap();
    sh(repo.path(), "git", &["merge", "--squash", "a"]);
    repo.stage_all().unwrap();
    repo.commit("feat: A, squashed", "", false).unwrap();
    repo.checkout("b").unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    assert!(!stack.entry("a").unwrap().merged, "a squash is invisible locally");

    // GitHub is what knows; `drop_and_restack` is handed that answer.
    let report = stack::drop_and_restack(&repo, &stack, &["a".to_string()]).unwrap();
    assert_eq!(report.restack.conflicted, None, "{:?}", report.log);
    assert_eq!(report.dropped, ["a"]);

    // `b` sits on the squashed trunk carrying only its own commit. Replaying
    // A1 and A2 on top of a commit that already contains them is the failure
    // this exists to prevent.
    assert_eq!(subjects(&repo, "b"), ["feat: B1"]);
    assert_eq!(stack::parent_of(&repo, "b").as_deref(), Some("main"));
    let all: Vec<String> =
        repo.log(20, Some("b")).unwrap().iter().map(|c| c.subject.clone()).collect();
    assert!(!all.contains(&"feat: A1".to_string()), "A1 was replayed: {all:?}");
}

/// The discriminating case: a squash merge whose content is not identical to
/// the branch's commits, because something was changed during review.
///
/// Git drops a replayed commit that turns out to be empty, which hides the
/// problem when the squash is byte-identical. Once a reviewer's tweak goes in
/// with the merge, the replayed commit is not empty — it conflicts with the
/// change it is a duplicate of. Rebasing from where the merged branch *ended*
/// never replays it at all.
#[test]
fn a_squash_merge_that_was_tweaked_in_review_still_rebases_cleanly() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");
    commit(&repo, "a1.txt", "one\n", "feat: A1");
    branch_on(&repo, "b", "a");
    commit(&repo, "b.txt", "b\n", "feat: B1");

    // Squash-merged with a change applied at merge time.
    repo.checkout("main").unwrap();
    commit(&repo, "a1.txt", "one, tweaked in review\n", "feat: A, squashed");
    repo.checkout("b").unwrap();

    let stack = stack::stack_for(&repo, "b").unwrap();
    let report = stack::drop_and_restack(&repo, &stack, &["a".to_string()]).unwrap();

    assert_eq!(report.restack.conflicted, None, "conflicted: {:?}", report.log);
    assert_eq!(subjects(&repo, "b"), ["feat: B1"], "A1 was replayed onto its own change");
    assert_eq!(read(&repo, "a1.txt"), "one, tweaked in review\n", "the review tweak survived");
}

#[test]
fn dropping_the_bottom_of_a_three_branch_stack_keeps_the_rest_in_order() {
    let (_tmp, repo) = setup();
    branch_on(&repo, "a", "main");
    commit(&repo, "a.txt", "a\n", "feat: A");
    branch_on(&repo, "b", "a");
    commit(&repo, "b.txt", "b\n", "feat: B");
    branch_on(&repo, "c", "b");
    commit(&repo, "c.txt", "c\n", "feat: C");

    repo.checkout("main").unwrap();
    sh(repo.path(), "git", &["merge", "--squash", "a"]);
    repo.stage_all().unwrap();
    repo.commit("feat: A, squashed", "", false).unwrap();
    repo.checkout("c").unwrap();

    let stack = stack::stack_for(&repo, "c").unwrap();
    let report = stack::drop_and_restack(&repo, &stack, &["a".to_string()]).unwrap();
    assert_eq!(report.restack.conflicted, None, "{:?}", report.log);

    assert_eq!(subjects(&repo, "b"), ["feat: B"]);
    assert_eq!(subjects(&repo, "c"), ["feat: B", "feat: C"]);
    let names: Vec<String> =
        stack::stack_for(&repo, "c").unwrap().entries.iter().map(|e| e.branch.clone()).collect();
    assert_eq!(names, ["b", "c"]);
}
