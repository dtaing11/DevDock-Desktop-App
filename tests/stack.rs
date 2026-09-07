//! Stacked pull requests through `gh stack`: reading the chain, growing it,
//! restacking, and the offline half of publishing.
//!
//! These need the GitHub CLI and its stack extension on the machine; without
//! them every test says so and passes, since the module's whole job is to
//! drive that extension. Nothing here talks to GitHub: the remote is a bare
//! repository on disk, which is enough for `gh stack push` and for the fetch
//! that `gh stack rebase` does.

use git_manage::git::Repo;
use git_manage::stack;
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

/// Runs `gh stack <args>` directly, for setting up what a test then reads.
fn gh(dir: &Path, args: &[&str]) {
    let program = stack::gh_program().expect("gh was found a moment ago");
    let out = Command::new(program)
        .arg("stack")
        .args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "gh stack {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every test starts here; without the tooling there is nothing to test.
macro_rules! require_gh_stack {
    () => {
        if !stack::available() {
            eprintln!("skipped: gh stack is not installed ({})", stack::INSTALL_HINT);
            return;
        }
    };
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
    // The trunk exists on the remote, so `gh stack rebase` has something to
    // fetch.
    repo.push_branch("main", false, None).unwrap();
    (tmp, repo)
}

fn commit(repo: &Repo, name: &str, content: &str, message: &str) {
    fs::write(repo.path().join(name), content).unwrap();
    repo.stage_all().unwrap();
    repo.commit(message, "", false).unwrap();
}

/// A plain branch off `parent`, not yet known to gh.
fn branch_off(repo: &Repo, name: &str, parent: &str) {
    repo.checkout(parent).unwrap();
    repo.create_branch(name, true).unwrap();
}

/// Subjects of `main..branch`, oldest first.
fn subjects(repo: &Repo, branch: &str) -> Vec<String> {
    let range = format!("main..{branch}");
    let mut v: Vec<String> =
        repo.log(50, Some(&range)).unwrap().iter().map(|c| c.subject.clone()).collect();
    v.reverse();
    v
}

fn names(stack: &stack::Stack) -> Vec<&str> {
    stack.entries.iter().map(|e| e.branch.as_str()).collect()
}

fn current(repo: &Repo) -> stack::Stack {
    stack::stack_for(repo, &repo.current_branch(), None).unwrap()
}

/// main → a(A1) → b(B1), tracked, checked out on `b`.
fn two_high(repo: &Repo) {
    branch_off(repo, "a", "main");
    commit(repo, "a.txt", "a1\n", "feat: A1");
    branch_off(repo, "b", "a");
    commit(repo, "b.txt", "b1\n", "feat: B1");
    gh(repo.path(), &["init", "--base", "main", "a", "b"]);
}

#[test]
fn an_untracked_branch_is_reported_not_invented() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    repo.create_branch("solo", true).unwrap();
    commit(&repo, "s.txt", "s\n", "feat: solo");

    let stack = current(&repo);
    assert!(!stack.tracked);
    assert!(stack.is_empty(), "gh does not know this branch, so neither do we");
    assert_eq!(stack.trunk, "main");
    assert_eq!(stack.tip(), "main", "a new branch here would start on the trunk");
    assert!(stack.legacy.is_empty());

    // The trunk itself is not an entry: it is what entries are based on.
    repo.checkout("main").unwrap();
    assert!(current(&repo).is_empty());
}

#[test]
fn stack_for_reads_the_chain_from_any_member() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    branch_off(&repo, "a", "main");
    commit(&repo, "a.txt", "a1\n", "feat: A1");
    branch_off(&repo, "b", "a");
    commit(&repo, "b.txt", "b1\n", "feat: B1");
    branch_off(&repo, "c", "b");
    commit(&repo, "c.txt", "c1\n", "feat: C1");
    gh(repo.path(), &["init", "--base", "main", "a", "b", "c"]);
    repo.checkout("b").unwrap();

    // Asked about the middle branch, the whole chain comes back.
    let stack = current(&repo);
    assert!(stack.tracked);
    assert_eq!(stack.trunk, "main");
    assert_eq!(names(&stack), ["a", "b", "c"], "bottom first");
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
    assert!(!stack.entries.iter().any(|e| e.pr.is_some()), "nothing is submitted");
}

#[test]
fn a_branch_on_the_tip_starts_a_stack_and_then_grows_it() {
    require_gh_stack!();
    let (_tmp, repo) = setup();

    // From the trunk: the new branch is a stack of one.
    let stack = current(&repo);
    let message = stack::branch_on_tip(&repo, &stack, "one", None).unwrap();
    assert!(message.contains("one") && message.contains("main"), "{message}");
    assert_eq!(repo.current_branch(), "one", "the new branch is checked out");
    commit(&repo, "one.txt", "1\n", "feat: one");

    let stack = current(&repo);
    assert!(stack.tracked);
    assert_eq!(names(&stack), ["one"]);
    assert_eq!(stack.entries[0].parent, "main");

    // From a tracked branch: added on top.
    stack::branch_on_tip(&repo, &stack, "two", None).unwrap();
    assert_eq!(repo.current_branch(), "two");
    commit(&repo, "two.txt", "2\n", "feat: two");

    // From lower in the stack: still added on top, since that is what "on
    // the tip" means — gh refuses to add anywhere else.
    repo.checkout("one").unwrap();
    let stack = current(&repo);
    stack::branch_on_tip(&repo, &stack, "three", None).unwrap();
    let stack = current(&repo);
    assert_eq!(names(&stack), ["one", "two", "three"]);
    assert_eq!(stack.entries[2].parent, "two");
    assert_eq!(repo.current_branch(), "three");
}

#[test]
fn a_branch_on_the_tip_of_an_untracked_branch_tracks_both() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    repo.create_branch("loose", true).unwrap();
    commit(&repo, "l.txt", "l\n", "feat: loose");

    let stack = current(&repo);
    assert!(!stack.tracked);
    stack::branch_on_tip(&repo, &stack, "on-top", None).unwrap();

    let stack = current(&repo);
    assert_eq!(names(&stack), ["loose", "on-top"]);
    assert_eq!(stack.entries[1].parent, "loose");
    assert_eq!(stack.entries[0].commits.len(), 1, "loose kept its commit as its own");
}

#[test]
fn a_new_branch_must_not_already_exist() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);
    let stack = current(&repo);
    let err = stack::branch_on_tip(&repo, &stack, "a", None).unwrap_err().to_string();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(names(&current(&repo)), ["a", "b"], "the stack was left alone");
}

#[test]
fn track_adopts_the_checked_out_branch() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    repo.create_branch("solo", true).unwrap();
    commit(&repo, "s.txt", "s\n", "feat: solo");

    let stack = current(&repo);
    stack::track(&repo, &stack, None).unwrap();
    let stack = current(&repo);
    assert!(stack.tracked);
    assert_eq!(names(&stack), ["solo"]);

    // The trunk cannot be tracked: it is what a stack is based on.
    repo.checkout("main").unwrap();
    let on_trunk = stack::Stack { trunk: "main".into(), ..Default::default() };
    assert!(stack::track(&repo, &on_trunk, None).is_err());
}

#[test]
fn restack_replays_only_each_branch_own_commits() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);

    // A new commit lands on the bottom branch; everything above is now stale.
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");

    let stack = current(&repo);
    assert!(stack.entry("b").unwrap().needs_restack);
    assert!(!stack.entry("a").unwrap().needs_restack);

    let report = stack::restack(&repo, &stack, None).unwrap();
    assert_eq!(report.conflicted, None);
    assert_eq!(report.moved, ["b"], "only the stale branch moved: {:?}", report.log);
    assert!(report.message().contains("b"), "{}", report.message());

    // B1 sits on top of both of a's commits, and A1 appears exactly once.
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: A2", "feat: B1"]);
    assert_eq!(repo.current_branch(), "a", "the checked-out branch is restored");
    assert!(current(&repo).stale().is_empty());
}

#[test]
fn restack_rebuilds_the_whole_chain_when_the_trunk_moves() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);

    repo.checkout("main").unwrap();
    commit(&repo, "m.txt", "m\n", "feat: M1");
    repo.checkout("b").unwrap();

    let stack = current(&repo);
    assert!(stack.entry("a").unwrap().needs_restack, "a is behind main");

    let report = stack::restack(&repo, &stack, None).unwrap();
    assert_eq!(report.conflicted, None);
    assert_eq!(report.moved, ["a", "b"]);

    // Both branches rebuilt on the new main, each commit still exactly once.
    assert_eq!(subjects(&repo, "a"), ["feat: A1"]);
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: B1"]);
    assert_eq!(repo.current_branch(), "b");
    // The local trunk, ahead of the remote, was used as it is.
    assert_eq!(repo.log(1, Some("main")).unwrap()[0].subject, "feat: M1");
}

#[test]
fn restack_is_a_noop_when_the_stack_is_in_order() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);

    let stack = current(&repo);
    let before = repo.git(&["rev-parse", "b"]).unwrap();
    let report = stack::restack(&repo, &stack, None).unwrap();

    assert!(report.moved.is_empty(), "{:?}", report.log);
    assert_eq!(report.message(), "Stack is already in order.");
    assert_eq!(repo.git(&["rev-parse", "b"]).unwrap(), before, "no rewrite");
}

#[test]
fn restack_refuses_a_dirty_working_tree() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");
    // A modified tracked file. An untracked one would be fine.
    fs::write(repo.path().join("a.txt"), "edited, not committed\n").unwrap();

    let stack = current(&repo);
    let err = stack::restack(&repo, &stack, None).unwrap_err().to_string();
    assert!(err.contains("uncommitted"), "unhelpful message: {err}");
    // Nothing was rewritten on the way to refusing.
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: B1"]);
}

#[test]
fn restack_stops_on_a_conflict_and_can_be_continued() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    branch_off(&repo, "a", "main");
    commit(&repo, "shared.txt", "from a\n", "feat: A1");
    branch_off(&repo, "b", "a");
    commit(&repo, "shared.txt", "from b\n", "feat: B1");
    branch_off(&repo, "c", "b");
    commit(&repo, "c.txt", "c\n", "feat: C1");
    gh(repo.path(), &["init", "--base", "main", "a", "b", "c"]);

    // Rewrite a's commit so b's change no longer applies cleanly.
    repo.checkout("a").unwrap();
    fs::write(repo.path().join("shared.txt"), "from a, revised\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("feat: A1 revised", "", true).unwrap();

    let stack = current(&repo);
    let report = stack::restack(&repo, &stack, None).unwrap();
    assert_eq!(report.conflicted.as_deref(), Some("b"), "{:?}", report.log);
    assert!(report.message().contains("conflicts"), "{}", report.message());

    // The rebase is still in progress, so the conflict resolver can finish it.
    assert!(!repo.conflicts().unwrap().is_empty(), "conflicts are on disk");
    assert!(stack::restack_in_progress(&repo));
    // HEAD is detached on the conflict, so gh cannot show the stack; the
    // view still says a restack is waiting, which is what the dialog needs.
    let stopped = stack::stack_for(&repo, "", None).unwrap();
    assert!(stopped.restacking);
    assert!(stopped.tracked);
    let err = stack::restack(&repo, &stack, None).unwrap_err().to_string();
    assert!(err.contains("already"), "a second restack must not pile on: {err}");

    // Resolve the way the app's resolver does — stage, `git rebase
    // --continue` — then carry on with the rest of the stack.
    fs::write(repo.path().join("shared.txt"), "from a, revised, and b\n").unwrap();
    repo.stage_all().unwrap();
    assert!(repo.rebase_continue().ok, "git could not continue");
    let report = stack::restack_continue(&repo, None).unwrap();
    assert_eq!(report.conflicted, None, "{:?}", report.log);
    assert!(!stack::restack_in_progress(&repo));

    assert_eq!(subjects(&repo, "c"), ["feat: A1 revised", "feat: B1", "feat: C1"]);
    let after = current(&repo);
    assert!(after.stale().is_empty(), "{:?}", after.entries);
    assert!(!after.restacking);
}

#[test]
fn a_stopped_restack_can_be_abandoned() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    branch_off(&repo, "a", "main");
    commit(&repo, "shared.txt", "from a\n", "feat: A1");
    branch_off(&repo, "b", "a");
    commit(&repo, "shared.txt", "from b\n", "feat: B1");
    gh(repo.path(), &["init", "--base", "main", "a", "b"]);
    let b_before = repo.git(&["rev-parse", "b"]).unwrap();

    repo.checkout("a").unwrap();
    fs::write(repo.path().join("shared.txt"), "from a, revised\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("feat: A1 revised", "", true).unwrap();

    let report = stack::restack(&repo, &current(&repo), None).unwrap();
    assert_eq!(report.conflicted.as_deref(), Some("b"));

    stack::restack_abort(&repo, None).unwrap();
    assert!(!stack::restack_in_progress(&repo));
    assert!(repo.conflicts().unwrap().is_empty());
    assert_eq!(repo.git(&["rev-parse", "b"]).unwrap(), b_before, "b is back where it was");
    assert_eq!(repo.current_branch(), "a");
    assert!(current(&repo).entry("b").unwrap().needs_restack, "and still behind");
}

#[test]
fn a_landed_branch_is_seen_as_merged() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);

    // Not merged yet.
    assert!(!current(&repo).entry("a").unwrap().merged);

    // Land `a` on main the way a rebase-merge would: same change, new sha.
    repo.checkout("main").unwrap();
    let sha = repo.git(&["rev-parse", "a"]).unwrap();
    sh(repo.path(), "git", &["cherry-pick", sha.trim()]);
    repo.checkout("b").unwrap();

    let stack = current(&repo);
    assert!(stack.entry("a").unwrap().merged, "cherry compares patches, not shas");
    let b = stack.entry("b").unwrap();
    assert!(!b.merged, "b has not landed");
    assert_eq!(b.parent, "main", "what sat on the merged branch is now based on the trunk");
    assert!(b.commits.iter().any(|c| c.subject == "feat: B1"), "{:?}", b.commits);
}

#[test]
fn push_stack_publishes_every_branch_without_checking_them_out() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("a").unwrap();

    let log = stack::push_stack(&repo, None).unwrap();
    assert!(log.iter().any(|l| l.contains("Pushed 2")), "{log:?}");
    assert_eq!(repo.current_branch(), "a", "pushing did not move HEAD");
    let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
    assert!(remote.contains("refs/heads/a"), "{remote}");
    assert!(remote.contains("refs/heads/b"), "{remote}");
}

#[test]
fn push_stack_updates_a_restacked_branch() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);
    stack::push_stack(&repo, None).unwrap();

    // Rewrite the bottom branch, restack, and publish again: the
    // force-with-lease push is what makes the second publish land at all.
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");
    stack::restack(&repo, &current(&repo), None).unwrap();
    stack::push_stack(&repo, None).unwrap();

    let local = repo.git(&["rev-parse", "b"]).unwrap();
    let remote = repo.git(&["rev-parse", "refs/remotes/origin/b"]).unwrap();
    assert_eq!(local, remote);
}

#[test]
fn submit_refuses_a_stale_stack_before_touching_the_network() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);
    repo.checkout("a").unwrap();
    commit(&repo, "a2.txt", "a2\n", "feat: A2");

    let stack = current(&repo);
    let err = stack::submit(&repo, &stack, false, None).unwrap_err().to_string();
    assert!(err.contains("Restack first"), "{err}");
    let remote = repo.git(&["ls-remote", "--heads", "origin"]).unwrap();
    assert!(!remote.contains("refs/heads/b"), "nothing was pushed: {remote}");
}

#[test]
fn untrack_forgets_the_stack_and_keeps_the_branches() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    two_high(&repo);

    stack::untrack(&repo, None).unwrap();
    let stack = current(&repo);
    assert!(!stack.tracked);
    assert!(stack.is_empty());
    assert_eq!(subjects(&repo, "b"), ["feat: A1", "feat: B1"], "the branches are untouched");
}

#[test]
fn a_chain_recorded_by_the_old_implementation_is_offered_and_imported() {
    require_gh_stack!();
    let (_tmp, repo) = setup();
    branch_off(&repo, "a", "main");
    commit(&repo, "a.txt", "a1\n", "feat: A1");
    branch_off(&repo, "b", "a");
    commit(&repo, "b.txt", "b1\n", "feat: B1");
    branch_off(&repo, "c", "b");
    commit(&repo, "c.txt", "c1\n", "feat: C1");
    // What DevDock wrote before it used gh.
    repo.git(&["config", "branch.a.devdock-parent", "main"]).unwrap();
    repo.git(&["config", "branch.b.devdock-parent", "a"]).unwrap();
    repo.git(&["config", "branch.b.devdock-pr", "11"]).unwrap();
    repo.git(&["config", "branch.c.devdock-parent", "b"]).unwrap();
    repo.checkout("b").unwrap();

    let stack = current(&repo);
    assert!(!stack.tracked);
    assert_eq!(stack.legacy, ["a", "b", "c"], "the whole chain, from the middle");

    stack::import_legacy(&repo, &stack, None).unwrap();
    let stack = current(&repo);
    assert!(stack.tracked);
    assert_eq!(names(&stack), ["a", "b", "c"]);
    assert_eq!(stack.current, Some(1), "the checkout stayed on b");
    for e in &stack.entries {
        assert_eq!(e.commits.len(), 1, "{} carried {:?}", e.branch, e.commits);
    }
    assert!(stack.legacy.is_empty());
    assert!(repo.git(&["config", "--get", "branch.b.devdock-parent"]).is_err(), "old record kept");
    assert!(repo.git(&["config", "--get", "branch.b.devdock-pr"]).is_err());
}
