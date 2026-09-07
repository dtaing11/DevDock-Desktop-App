//! The stacked-pull-request flow against real GitHub, through `gh stack`.
//!
//! Ignored by default, like the other live tests: it needs a token, it makes
//! network calls, and it creates a repository.
//!
//! ```
//! cargo test --test stack_live -- --ignored --nocapture
//! ```
//!
//! The token comes from `GITHUB_TOKEN`, or from the app's own store if you are
//! signed in, and needs `repo` scope to create the scratch repository. The
//! repository is left behind on purpose: deleting one needs the `delete_repo`
//! scope, and a library that can delete repositories is a worse thing to have
//! than a scratch repository to tidy up. The URL is printed at the end.

use git_manage::git::Repo;
use git_manage::github::{Client, RepoSlug, TokenStore};
use git_manage::stack;
use std::fs;
use std::path::Path;
use std::process::Command;

fn token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()).or_else(TokenStore::load)
}

fn sh(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(repo: &Repo, name: &str, content: &str, message: &str) {
    fs::write(repo.path().join(name), content).unwrap();
    repo.stage_all().unwrap();
    repo.commit(message, "", false).unwrap();
}

fn current(repo: &Repo, auth: Option<&str>) -> stack::Stack {
    stack::stack_for(repo, &repo.current_branch(), auth).unwrap()
}

#[test]
#[ignore = "creates a repository on GitHub"]
fn a_stack_is_submitted_merged_and_synced() {
    assert!(stack::available(), "gh stack is not installed: {}", stack::INSTALL_HINT);
    let Some(token) = token() else {
        panic!("no GitHub token: set GITHUB_TOKEN or sign in through the app");
    };
    let client = Client::new(token.clone());
    let login = client.user().expect("could not read the account").login;

    // A scratch repository, named so an abandoned one is obvious.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let name = format!("devdock-stack-live-{stamp}");
    let slug = RepoSlug { owner: login.clone(), repo: name.clone() };
    client.create_repo(&name, true).expect("could not create the scratch repository");
    println!("scratch repository: https://github.com/{slug}");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&client, &slug, &token);
    }));

    println!("\ndone. delete the scratch repository at:");
    println!("  https://github.com/{slug}/settings");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn run(client: &Client, slug: &RepoSlug, token: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    sh(&work, &["init", "-b", "main"]);
    sh(&work, &["config", "user.email", "devdock-test@example.invalid"]);
    sh(&work, &["config", "user.name", "DevDock live test"]);
    sh(&work, &["remote", "add", "origin", &format!("https://github.com/{slug}.git")]);
    let repo = Repo::open(&work).unwrap();
    let auth = Some(token);

    commit(&repo, "README.md", "# scratch\n", "chore: root");
    repo.push_branch("main", false, auth).unwrap();

    // main → one → two → three, built the way the app builds them. The
    // bottom branch gets two commits, so the squash merge below produces one
    // commit whose patch matches neither of them — the case where nothing
    // local can tell that it landed.
    let stack0 = current(&repo, auth);
    stack::branch_on_tip(&repo, &stack0, "one", auth).unwrap();
    commit(&repo, "one.txt", "one\n", "feat: the first piece");
    commit(&repo, "one-more.txt", "and more\n", "feat: the first piece, continued");
    stack::branch_on_tip(&repo, &current(&repo, auth), "two", auth).unwrap();
    commit(&repo, "two.txt", "two\n", "feat: the second piece");
    stack::branch_on_tip(&repo, &current(&repo, auth), "three", auth).unwrap();
    commit(&repo, "three.txt", "three\n", "feat: the third piece");

    // -- submit -------------------------------------------------------------

    let built = current(&repo, auth);
    let report = stack::submit(&repo, &built, true, auth).expect("submit failed");
    println!("submit: {}", report.message());
    for line in &report.log {
        println!("  {line}");
    }
    assert_eq!(report.opened, 3, "three pull requests, one per branch");
    assert_eq!(report.total, 3);

    let open = client.pull_requests(slug).unwrap();
    let find = |head: &str| {
        open.iter().find(|pr| pr.head == head).unwrap_or_else(|| panic!("no PR for {head}")).clone()
    };
    let (pr_one, pr_two, pr_three) = (find("one"), find("two"), find("three"));

    // Each targets the branch below it. This is the whole feature.
    assert_eq!(pr_one.base, "main");
    assert_eq!(pr_two.base, "one");
    assert_eq!(pr_three.base, "two");

    // GitHub shows each PR only what it introduces.
    let files = client.pr_files(slug, pr_two.number).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["two.txt"], "#{} showed {paths:?}", pr_two.number);
    // And the bottom one shows both of its own, and nothing above it.
    let mut paths: Vec<String> = client
        .pr_files(slug, pr_one.number)
        .unwrap()
        .iter()
        .map(|f| f.path.clone())
        .collect();
    paths.sort();
    assert_eq!(paths, ["one-more.txt", "one.txt"]);

    // gh remembered the numbers, so the view shows them.
    let submitted = current(&repo, auth);
    assert_eq!(submitted.entry("one").unwrap().pr, Some(pr_one.number));
    assert_eq!(submitted.entry("three").unwrap().pr, Some(pr_three.number));

    // Re-submitting must not open anything again.
    let report = stack::submit(&repo, &submitted, true, auth).expect("re-submit failed");
    assert_eq!(report.opened, 0, "a second submit opened more pull requests");
    assert_eq!(client.pull_requests(slug).unwrap().len(), 3);

    // -- merge the bottom one -----------------------------------------------

    // Squash, the hard case: the merge commit's patch matches none of the
    // branch's own, so nothing local can tell that it landed.
    client
        .merge_pull_request(slug, pr_one.number, "squash")
        .expect("could not squash-merge the bottom PR");
    println!("squash-merged #{}", pr_one.number);
    sh(&work, &["checkout", "three"]);

    // -- sync ---------------------------------------------------------------

    let sync = stack::sync(&repo, auth).expect("sync failed");
    println!("sync: {}", sync.message());
    for line in &sync.log {
        println!("  {line}");
    }
    assert_eq!(sync.merged, ["one"], "GitHub's answer is what caught the squash");

    let after = current(&repo, auth);
    assert!(after.entry("one").unwrap().merged);
    assert_eq!(after.entry("two").unwrap().parent, "main", "two now sits on the trunk");
    assert!(after.stale().is_empty(), "sync rebased what was left: {:?}", after.entries);
    // Each branch still carries only its own commit: nothing was replayed.
    for (branch, subject) in
        [("two", "feat: the second piece"), ("three", "feat: the third piece")]
    {
        let commits = after.entry(branch).unwrap().commits.clone();
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, [subject], "{branch} carries {subjects:?}");
    }
    // And GitHub agrees: the pull request above the merged one targets main.
    let two = client.pull_request(slug, pr_two.number).unwrap();
    assert_eq!(two.pr.base, "main", "#{} was not retargeted", pr_two.number);
}
