//! The `devdock` command-line interface.
//!
//! Running `devdock` with no arguments launches the GUI; any subcommand
//! runs headlessly. See `devdock help` or docs/cli.md.

use crate::git::Repo;
use std::path::PathBuf;
use std::process::ExitCode;

/// Entry point for CLI mode. Returns `None` when no subcommand was given
/// (caller should launch the GUI), otherwise the process exit code.
pub fn run(args: &[String]) -> Option<ExitCode> {
    let cmd = args.first().map(String::as_str)?;
    let rest = &args[1..];
    let code = match cmd {
        "ci" => cmd_ci(),
        "status" => cmd_status(),
        "log" => cmd_log(rest),
        "branches" => cmd_branches(),
        "stash" => cmd_stash(rest),
        "push" => cmd_push(rest),
        "commit" => cmd_commit(rest),
        "pr" => cmd_pr(rest),
        "hook" => cmd_hook(rest),
        "help" | "--help" | "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        "--version" | "-V" => {
            println!("devdock {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("devdock: unknown command \"{other}\"\n");
            print_help();
            ExitCode::from(2)
        }
    };
    Some(code)
}

fn print_help() {
    println!(
        "devdock — native git client with local CI and AI commit messages

USAGE:
    devdock              launch the GUI
    devdock <command>    run a command headlessly

COMMANDS:
    status               working-tree summary (branch, ahead/behind, files)
    log [N]              last N commits (default 10)
    branches             local and remote branches, current marked
    stash list           stashes with their origin branch
    stash save [MSG]     stash all changes (incl. untracked)
    stash pop [INDEX]    apply and drop a stash (default newest)
    commit -m MSG        stage everything and commit
    commit --ai          AI-generated message (uses the app's configured model)
    push                 push current branch, gated by local CI when configured
    push --force         force push (--force-with-lease)
    push --no-verify     skip the local CI gate
    pr -t TITLE [-b BODY]   push branch and open a pull request into the
                         default branch (main/master), gated by local CI
    pr --ai              AI-generated PR title and body
    ci                   run all local CI jobs (.git-manage-ci.toml)
    hook install         install the git pre-push hook running `devdock ci`
    hook remove          remove the pre-push hook
    hook status          show whether the hook is installed
    help                 this text

Docs: docs/cli.md · Local CI: docs/local-ci.md"
    );
}

/// Opens the repository containing the current directory.
fn repo() -> Result<Repo, ExitCode> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("devdock: {e}");
        ExitCode::FAILURE
    })?;
    Repo::open(&cwd).map_err(|_| {
        eprintln!("devdock: not inside a git repository");
        ExitCode::FAILURE
    })
}

fn repo_root() -> Result<PathBuf, ExitCode> {
    Ok(repo()?.path().to_path_buf())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_ci() -> ExitCode {
    let Ok(root) = repo_root() else { return ExitCode::FAILURE };
    match crate::local_ci::run_all_cli(&root) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("devdock ci: checks failed");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("devdock ci: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_status() -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    match repo.status() {
        Ok(status) => {
            let sync = if !status.has_remote {
                "no remote".to_string()
            } else if !status.has_upstream {
                format!("unpublished, {} commit(s)", status.ahead)
            } else {
                format!("ahead {}, behind {}", status.ahead, status.behind)
            };
            println!("branch: {} ({sync})", status.branch);
            if status.files.is_empty() {
                println!("clean working tree");
            } else {
                for file in &status.files {
                    let mark = if file.conflicted {
                        "!"
                    } else if file.staged {
                        "+"
                    } else {
                        "*"
                    };
                    println!("  {mark} {}", file.path);
                }
                println!("{} changed file(s)  (+ staged, * unstaged, ! conflict)", status.files.len());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_log(rest: &[String]) -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    let n: u32 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10);
    match repo.log(n, None) {
        Ok(commits) => {
            for c in commits {
                println!(
                    "{}  {}  {} · {}",
                    c.short_sha,
                    c.subject,
                    c.author,
                    c.date.get(..10).unwrap_or(&c.date)
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_branches() -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    match repo.branches() {
        Ok(list) => {
            for b in &list.local {
                let marker = if b.current { "*" } else { " " };
                println!("{marker} {}", b.name);
            }
            for b in &list.remote {
                println!("  {} (remote)", b.name);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_stash(rest: &[String]) -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    match rest.first().map(String::as_str) {
        Some("list") | None => match repo.stash_list() {
            Ok(stashes) if stashes.is_empty() => {
                println!("no stashes");
                ExitCode::SUCCESS
            }
            Ok(stashes) => {
                for s in stashes {
                    let branch = s.branch.as_deref().unwrap_or("?");
                    println!("  [{}] ({branch}) {}", s.index, s.message);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("devdock: {e}");
                ExitCode::FAILURE
            }
        },
        Some("save") => {
            let message = rest.get(1..).map(|r| r.join(" ")).unwrap_or_default();
            match repo.stash_save(&message) {
                Ok(()) => {
                    println!("stashed");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("devdock: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("pop") => {
            let index: u32 = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            match repo.stash_pop(index) {
                Ok(()) => {
                    println!("stash applied");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("devdock: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("devdock stash: unknown subcommand \"{other}\" (list/save/pop)");
            ExitCode::from(2)
        }
    }
}

fn cmd_push(rest: &[String]) -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    let force = rest.iter().any(|a| a == "--force");
    let no_verify = rest.iter().any(|a| a == "--no-verify");

    // Local CI gate (same semantics as the GUI and pre-push hook).
    match ci_gate(&repo, no_verify) {
        Ok(true) => {}
        Ok(false) => return ExitCode::FAILURE,
        Err(code) => return code,
    }

    let token = crate::github::TokenStore::load();
    let set_upstream = repo.status().map(|s| !s.has_upstream).unwrap_or(false);
    let result = if force {
        repo.force_push(token.as_deref())
    } else {
        repo.push(set_upstream, token.as_deref())
    };
    match result {
        Ok(_) => {
            println!("pushed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_commit(rest: &[String]) -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    if let Err(e) = repo.stage_all() {
        eprintln!("devdock: {e}");
        return ExitCode::FAILURE;
    }

    let message = if rest.first().map(String::as_str) == Some("--ai") {
        match ai_message(&repo) {
            Ok(suggestion) => {
                println!("AI message: {}", suggestion.summary);
                (suggestion.summary, suggestion.description)
            }
            Err(e) => {
                eprintln!("devdock: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else if rest.first().map(String::as_str) == Some("-m") {
        let Some(msg) = rest.get(1) else {
            eprintln!("devdock commit: -m requires a message");
            return ExitCode::from(2);
        };
        (msg.clone(), String::new())
    } else {
        eprintln!("devdock commit: use -m MSG or --ai");
        return ExitCode::from(2);
    };

    match repo.commit(&message.0, &message.1, false) {
        Ok(sha) => {
            println!("committed {}", &sha[..7]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Generates a commit message with the app's configured provider/model.
fn ai_message(repo: &Repo) -> Result<crate::ollama::CommitSuggestion, String> {
    let diff = repo.diff_for_ai().map_err(|e| e.to_string())?;
    if diff.trim().is_empty() {
        return Err("no changes to describe".into());
    }
    let config = crate::app::Config::load();

    // Prefer the commit-task selection, then legacy fields, then defaults.
    let (provider, model) = match &config.commit_ai {
        Some(sel) => (sel.provider.clone(), sel.model.clone()),
        None => {
            let provider = config.ai_provider.clone().unwrap_or_else(|| "ollama".into());
            let model = if provider == "claude" {
                config.claude_model.clone().unwrap_or_default()
            } else {
                config.ollama_model.clone().unwrap_or_default()
            };
            (provider, model)
        }
    };

    if provider == "claude" {
        let client = crate::claude::Client::from_store(model)
            .ok_or("Claude is not signed in (sign in from the GUI settings)")?;
        client.commit_message(&diff, None).map_err(|e| e.to_string())
    } else {
        if model.is_empty() {
            return Err("no Ollama model configured (pick one in the GUI)".into());
        }
        let url = config.ollama_url.unwrap_or_else(|| crate::ollama::DEFAULT_URL.into());
        crate::ollama::Client::new(url)
            .commit_message(&model, &diff, None)
            .map_err(|e| e.to_string())
    }
}

fn cmd_hook(rest: &[String]) -> ExitCode {
    let Ok(root) = repo_root() else { return ExitCode::FAILURE };
    match rest.first().map(String::as_str) {
        Some("install") => match crate::local_ci::install_pre_push_hook(&root) {
            Ok(()) => {
                println!("pre-push hook installed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("devdock: {e}");
                ExitCode::FAILURE
            }
        },
        Some("remove") => {
            let hook = root.join(".git").join("hooks").join("pre-push");
            if crate::local_ci::hook_installed(&root) {
                match std::fs::remove_file(&hook) {
                    Ok(()) => {
                        println!("pre-push hook removed");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("devdock: {e}");
                        ExitCode::FAILURE
                    }
                }
            } else {
                println!("no devdock hook installed");
                ExitCode::SUCCESS
            }
        }
        Some("status") | None => {
            if crate::local_ci::hook_installed(&root) {
                println!("pre-push hook: installed");
            } else {
                println!("pre-push hook: not installed");
            }
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("devdock hook: unknown subcommand \"{other}\" (install/remove/status)");
            ExitCode::from(2)
        }
    }
}

/// Runs the local CI gate (same semantics as push). Returns false when the
/// push/PR should be aborted.
fn ci_gate(repo: &Repo, no_verify: bool) -> Result<bool, ExitCode> {
    if no_verify {
        return Ok(true);
    }
    let Ok(Some(config)) = crate::local_ci::load_config(repo.path()) else {
        return Ok(true);
    };
    if !config.on_push.run || config.jobs.is_empty() {
        return Ok(true);
    }
    println!("devdock: running local checks…");
    match crate::local_ci::run_all_cli(repo.path()) {
        Ok(true) => Ok(true),
        Ok(false) if config.on_push.block_on_failure => {
            eprintln!("devdock: checks failed; aborted (--no-verify to skip)");
            Ok(false)
        }
        Ok(false) => {
            eprintln!("devdock: checks failed (non-blocking), continuing");
            Ok(true)
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// `devdock pr`: run CI gate, push the branch, and open a pull request
/// into the default branch.
fn cmd_pr(rest: &[String]) -> ExitCode {
    let repo = match repo() {
        Ok(r) => r,
        Err(code) => return code,
    };
    let no_verify = rest.iter().any(|a| a == "--no-verify");

    // 1. Local CI gate.
    match ci_gate(&repo, no_verify) {
        Ok(true) => {}
        Ok(false) => return ExitCode::FAILURE,
        Err(code) => return code,
    }

    // 2. Resolve title/body.
    let (title, body) = if rest.iter().any(|a| a == "--ai") {
        match ai_message(&repo) {
            Ok(s) => (s.summary, s.description),
            Err(e) => {
                eprintln!("devdock: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let title = rest
            .iter()
            .position(|a| a == "-t")
            .and_then(|i| rest.get(i + 1))
            .cloned();
        let Some(title) = title else {
            eprintln!("devdock pr: use -t TITLE (and optional -b BODY) or --ai");
            return ExitCode::from(2);
        };
        let body = rest
            .iter()
            .position(|a| a == "-b")
            .and_then(|i| rest.get(i + 1))
            .cloned()
            .unwrap_or_default();
        (title, body)
    };

    // 3. Push the branch (publishes if needed).
    let token = crate::github::TokenStore::load();
    let set_upstream = repo.status().map(|s| !s.has_upstream).unwrap_or(false);
    if let Err(e) = repo.push(set_upstream, token.as_deref()) {
        eprintln!("devdock: push failed: {e}");
        return ExitCode::FAILURE;
    }

    // 4. Create the PR into main/master.
    let Some(client) = crate::github::Client::from_store() else {
        eprintln!("devdock: not signed in to GitHub (sign in from the GUI)");
        return ExitCode::FAILURE;
    };
    let Some(slug) = repo
        .remotes()
        .ok()
        .and_then(|remotes| {
            remotes
                .iter()
                .find(|r| r.name == "origin")
                .or_else(|| remotes.first())
                .and_then(|r| crate::github::parse_remote(&r.url))
        })
    else {
        eprintln!("devdock: no github.com remote found");
        return ExitCode::FAILURE;
    };
    let head = repo.current_branch();
    let base = repo
        .branches()
        .ok()
        .and_then(|b| {
            b.local
                .iter()
                .find(|br| br.name == "main" || br.name == "master")
                .map(|br| br.name.clone())
        })
        .unwrap_or_else(|| "main".into());
    if head == base {
        eprintln!("devdock pr: already on {base}; switch to a feature branch first");
        return ExitCode::from(2);
    }
    match client.create_pull_request(&slug, &title, &body, &head, &base) {
        Ok(pr) => {
            println!("PR #{} created: {}", pr.number, pr.html_url);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}
