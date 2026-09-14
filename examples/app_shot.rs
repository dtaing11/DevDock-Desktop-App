//! Screenshots the app's own layout, so it can be looked at rather than
//! reasoned about.
//!
//! `cargo run --example app_shot -- <repo> <tab> <out.rgba> [width] [height]`
//! where tab is one of: changes, history, checks, editor, agent.
//!
//! `DIALOG=stack|worktrees|pr|tickets` opens a dialog over the app before the
//! picture is taken.
//! `AGENT_DEMO=running|changes` fills the coding agent with state, since a
//! real run needs a model and a screenshot needs neither.

use eframe::egui;
use git_manage::app::{views, App, ProposedEdit, Tab};

struct Shot {
    app: App,
    out: String,
    frame: u32,
    /// Opened after the repository has finished loading: opening it sooner
    /// is undone, because a repository change clears the editor.
    open: Option<String>,
    shoot_at: u32,
    /// Last observed size of the dialog named by `DIALOG_ID`.
    last_modal: Option<egui::Vec2>,
}

impl eframe::App for Shot {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.app.pump_for_tools();
        if self.frame == 5 {
            if let Some(file) = self.open.take() {
                self.app.editor_open(std::path::Path::new(&file), None);
            }
            if let Ok(mode) = std::env::var("AGENT_DEMO") {
                seed_agent(&mut self.app, &mode);
            }
            match std::env::var("DIALOG").as_deref() {
                Ok("stack") => self.app.open_stack(),
                Ok("worktrees") => self.app.open_worktrees(),
                Ok("pr") => self.app.dialog = git_manage::app::Dialog::PullRequests,
                Ok("tickets") => {
                    seed_tickets(&mut self.app);
                    self.app.dialog = git_manage::app::Dialog::Tickets;
                }
                Ok("tickets-connect") => {
                    self.app.dialog = git_manage::app::Dialog::Tickets;
                }
                _ => {}
            }
            if std::env::var("TERMINAL").is_ok() {
                self.app.terminal_open(false);
                if let Ok(command) = std::env::var("TERMINAL_CMD") {
                    self.app.terminal_run(&command);
                }
            }
        }

        // The same panels the app draws, in the same order, so a
        // screenshot is of the app rather than of part of it.
        views::toolbar(&mut self.app, ctx);
        views::sidebar(&mut self.app, ctx);
        // Bottom panel before the central one, as the app does it.
        #[cfg(unix)]
        git_manage::app::terminal_panel::panel(&mut self.app, ctx);
        views::diff_panel(&mut self.app, ctx);
        git_manage::app::dialogs::show(&mut self.app, ctx);
        views::toasts(&mut self.app, ctx);

        // The sidebar must not grow frame over frame: egui stores a panel's
        // width from its content, so a greedy child compounds.
        if self.frame < 12 || self.frame.is_multiple_of(40) {
            if let Some(state) = egui::containers::panel::PanelState::load(
                ctx,
                egui::Id::new("sidebar"),
            ) {
                print!("frame {}: sidebar {:.0}pt", self.frame, state.rect.width());
            }
            if let Some(state) = egui::containers::panel::PanelState::load(
                ctx,
                egui::Id::new("terminal"),
            ) {
                print!("  terminal {:.0}pt", state.rect.height());
            }
            println!();
        }

        // A modal is anchored from the size it had last frame, so one that is
        // still filling in is drawn from a stale centre and can hang off the
        // bottom for a frame. Printing the size makes that visible instead of
        // it looking like a layout bug in the screenshot.
        if let Ok(name) = std::env::var("DIALOG_ID") {
            if let Some(state) = egui::AreaState::load(ctx, egui::Id::new(name.as_str())) {
                if state.size != self.last_modal {
                    self.last_modal = state.size;
                    println!("frame {}: modal size {:?}", self.frame, state.size);
                }
            }
        }

        // Give background work (tracked files, language servers) a few
        // frames to land before the picture is taken.
        if self.frame == self.shoot_at {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        if self.frame > self.shoot_at {
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = shot {
                let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                std::fs::write(&self.out, &bytes).unwrap();
                std::fs::write(
                    format!("{}.size", self.out),
                    format!("{} {}", image.width(), image.height()),
                )
                .unwrap();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        ctx.request_repaint();
    }
}

/// Puts the ticket writer into a state worth photographing: connected, with
/// a list and the drafts a model wrote from it.
fn seed_tickets(app: &mut App) {
    use git_manage::agent::tickets::Draft;
    use git_manage::app::DraftedTicket;
    use git_manage::jira::{IssueType, Project};

    app.tickets.account = Some("Dina Taing".into());
    app.tickets.creds.site = "https://acme.atlassian.net".into();
    app.tickets.projects = vec![
        Project { id: "1".into(), key: "DEV".into(), name: "DevDock".into() },
        Project { id: "2".into(), key: "OPS".into(), name: "Operations".into() },
    ];
    app.tickets.project = "DEV".into();
    app.tickets.types = ["Task", "Bug", "Story"]
        .iter()
        .enumerate()
        .map(|(i, name)| IssueType {
            id: i.to_string(),
            name: (*name).into(),
            subtask: false,
        })
        .collect();
    app.tickets.list = "- add a --json flag to devdock status\n         - fix the crash when the repository has no commits\n         - document the local CI config"
        .into();
    app.tickets.notes = "The second one is a real crash; the others are small.".into();

    let draft = |summary: &str, kind: &str, description: &str, labels: &[&str], from: &str| {
        DraftedTicket {
            draft: Draft {
                summary: summary.into(),
                description: description.into(),
                issue_type: kind.into(),
                labels: labels.iter().map(|l| (*l).to_string()).collect(),
                source: vec![from.into()],
            },
            accepted: true,
            created: None,
            error: None,
        }
    };
    app.tickets.drafts = vec![
        draft(
            "Add a --json flag to devdock status",
            "Task",
            "`cmd_status` in `src/cli.rs` prints through `render()`. The status \
             struct already derives `Serialize`, so the flag only has to pick the \
             encoder.\n\n**Done when** `devdock status --json` prints the same \
             information as valid JSON, and a test asserts the parsed shape rather \
             than the bytes.",
            &["cli"],
            "add a --json flag to devdock status",
        ),
        draft(
            "Fix the crash when the repository has no commits",
            "Bug",
            "`Repo::log` returns an empty vector for a repository with no commits, \
             but `branch_summary` unwraps the first entry.\n\n**Done when** \
             opening a freshly initialised repository shows an empty history \
             instead of panicking.",
            &["crash"],
            "fix the crash when the repository has no commits",
        ),
        draft(
            "Document the local CI config",
            "Task",
            "`docs/local-ci.md` covers the jobs but not the `[review]` block that \
             sits in the same file.",
            &["docs"],
            "document the local CI config",
        ),
    ];
    app.tickets.drafts[2].created = Some(git_manage::jira::Issue {
        id: "10041".into(),
        key: "DEV-118".into(),
        url: "https://acme.atlassian.net/browse/DEV-118".into(),
    });
    app.tickets.drafts[2].accepted = false;
    app.tickets.expanded = Some(0);
}

/// Puts the coding agent into a state worth photographing.
fn seed_agent(app: &mut App, mode: &str) {
    use git_manage::agent::{PendingEdit, PlanStep};
    let step = |text: &str, done: bool| PlanStep { text: text.into(), done };
    app.tab = Tab::Agent;
    app.coding.task = "add a --json flag to devdock status and cover it with a test".into();
    app.coding.plan = vec![
        step("Read the CLI argument parser", true),
        step("Add a --json flag to `status`", true),
        step("Serialise the status struct", mode == "changes"),
        step("Write a test for the new output", false),
        step("Run the test suite", false),
    ];
    app.coding.log = vec![
        "· read src/cli.rs".into(),
        "· read src/git.rs".into(),
        "· plan: 2/5 done".into(),
        "· edit src/cli.rs".into(),
        "… the status struct already derives Serialize, so this is mostly wiring".into(),
        "· edit src/cli.rs".into(),
        "· run cargo test --lib cli".into(),
    ];
    app.coding.running = mode == "running";
    app.coding.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(74));
    if mode == "changes" {
        app.coding.running = false;
        app.coding.took = Some(std::time::Duration::from_secs(96));
        app.coding.summary = "Added a `--json` flag to `devdock status`.\n\n             The status struct already derived `Serialize`, so the flag only had to \
             pick the encoder — **no new types**. The test covers the flag's output \
             shape rather than its exact bytes, so field order cannot break it.\n\n             - `src/cli.rs` — the flag, and the branch that serialises\n             - `tests/workflow.rs` — one test, asserting the parsed JSON"
            .into();
        let edit = |path: &str, before: Option<&str>, after: &str| ProposedEdit {
            edit: PendingEdit {
                path: path.into(),
                before: before.map(str::to_string),
                after: after.into(),
            },
            accepted: false,
            applied: false,
            unresolved: false,
        };
        app.coding.edits = vec![
            edit(
                "src/cli.rs",
                Some("fn status(repo: &Repo) -> Result<()> {\n    let s = repo.status()?;\n    println!(\"{}\", render(&s));\n    Ok(())\n}\n"),
                "fn status(repo: &Repo, json: bool) -> Result<()> {\n    let s = repo.status()?;\n    if json {\n        println!(\"{}\", serde_json::to_string_pretty(&s)?);\n        return Ok(());\n    }\n    println!(\"{}\", render(&s));\n    Ok(())\n}\n",
            ),
            edit(
                "tests/workflow.rs",
                Some("#[test]\nfn status_reports_a_clean_tree() {\n    let (_tmp, repo) = setup();\n    assert!(repo.status().unwrap().files.is_empty());\n}\n"),
                "#[test]\nfn status_reports_a_clean_tree() {\n    let (_tmp, repo) = setup();\n    assert!(repo.status().unwrap().files.is_empty());\n}\n\n#[test]\nfn status_json_carries_the_branch_and_files() {\n    let (_tmp, repo) = setup();\n    let out = run_cli(&repo, &[\"status\", \"--json\"]);\n    let value: serde_json::Value = serde_json::from_str(&out).unwrap();\n    assert_eq!(value[\"branch\"], \"main\");\n    assert!(value[\"files\"].is_array());\n}\n",
            ),
        ];
        app.coding.selected = Some(0);
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let repo = args.get(1).cloned().unwrap_or_else(|| ".".into());
    let tab = args.get(2).cloned().unwrap_or_else(|| "changes".into());
    let out = args.get(3).cloned().unwrap_or_else(|| "/tmp/app.rgba".into());
    let width: f32 = args.get(4).and_then(|w| w.parse().ok()).unwrap_or(1500.0);
    let height: f32 = args.get(5).and_then(|h| h.parse().ok()).unwrap_or(950.0);
    let open = args.get(6).cloned();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([width, height]),
        ..Default::default()
    };
    eframe::run_native(
        "app_shot",
        options,
        Box::new(move |cc| {
            if std::env::var("LIGHT").is_ok() {
                git_manage::app::theme::set_light(true);
            }
            git_manage::app::theme::apply(&cc.egui_ctx);
            let mut app = App::new_bare(&cc.egui_ctx);
            app.open_repo(&repo);
            app.tab = match tab.as_str() {
                "history" => Tab::History,
                "checks" => Tab::Checks,
                "editor" => Tab::Editor,
                "agent" => Tab::Agent,
                _ => Tab::Changes,
            };
            let shoot_at: u32 = std::env::var("SHOOT_AT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            Ok(Box::new(Shot {
                app,
                out,
                frame: 0,
                open: open.clone(),
                shoot_at,
                last_modal: None,
            }))
        }),
    )
}
