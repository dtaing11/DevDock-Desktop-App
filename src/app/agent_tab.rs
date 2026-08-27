//! The coding agent's tab: a task, a live account of what the model is
//! doing, and its changes waiting to be confirmed.
//!
//! The panel is a conversation, not a one-shot form. Each task and the
//! summary it produced stay listed, and the next task is sent with them, so
//! "now do the same for the other module" means something.
//!
//! # What "confirm" means here
//!
//! Both write modes end in the same review — a diff per file, ticked
//! individually — but they differ in what the tick does:
//!
//! - **Propose** ([`crate::agent::WriteMode::Overlay`]): nothing is on disk. Ticking a file
//!   and applying writes it.
//! - **Let it iterate** ([`crate::agent::WriteMode::Live`]): the files are already
//!   written, because the model needed to compile and test them. Ticking a file and
//!   reverting restores exactly what was there before the run.
//!
//! The wording in the panel changes with the mode, because "Apply" and
//! "Revert" are not the same promise.

use super::worker::AiTarget;
use super::{theme, App, ProposedEdit};
use egui::{RichText, ScrollArea};

/// One completed exchange, shown in the transcript.
pub struct Exchange {
    pub task: String,
    pub summary: String,
    /// How many files it changed, for the collapsed line.
    pub changed: usize,
}

/// Everything the agent tab owns.
#[derive(Default)]
pub struct CodingState {
    /// The task being typed.
    pub task: String,
    /// Whether the model may write to the worktree and run checks.
    pub iterate: bool,
    pub running: bool,
    /// Live progress from the current run.
    pub log: Vec<String>,
    /// The agent's own plan, ticked off as it works.
    pub plan: Vec<crate::agent::PlanStep>,
    /// Completed exchanges, oldest first.
    pub history: Vec<Exchange>,
    /// The latest run's closing summary.
    pub summary: String,
    /// Changes awaiting confirmation.
    pub edits: Vec<ProposedEdit>,
    pub selected: Option<usize>,
    pub error: Option<String>,
    pub truncated: bool,
    /// The write mode the pending changes were made under, which decides
    /// whether confirming means applying or keeping.
    pub live: bool,
}

impl CodingState {
    /// Turns the transcript into what the harness needs for follow-ups.
    pub fn turns(&self) -> Vec<crate::agent::coding::Turn> {
        self.history
            .iter()
            .map(|e| crate::agent::coding::Turn {
                task: e.task.clone(),
                summary: e.summary.clone(),
            })
            .collect()
    }

    pub fn pending_count(&self) -> usize {
        self.edits.iter().filter(|e| e.accepted && !e.applied).count()
    }

    /// Whether a run's changes are still waiting on the user.
    pub fn awaiting_review(&self) -> bool {
        self.edits.iter().any(|e| !e.applied)
    }
}

/// The agent's half of the sidebar: the task, what it is doing, and which
/// of its changes you are looking at. The diffs are in the viewport.
pub fn agent_sidebar(app: &mut App, ui: &mut egui::Ui) {
    if app.repo.is_none() {
        ui.label(RichText::new("Open a repository to use the coding agent.").color(theme::fg_dim()));
        return;
    }

    task_panel(app, ui);
    ui.separator();

    ScrollArea::vertical().auto_shrink([false, false]).id_salt("agent-sidebar").show(
        ui,
        |ui| {
            transcript(app, ui);
            plan(app, ui);
            if app.coding.running || !app.coding.log.is_empty() {
                activity(app, ui);
            }
            if let Some(error) = app.coding.error.clone() {
                ui.add_space(6.0);
                ui.label(RichText::new(error).color(theme::danger()));
            }
            if !app.coding.edits.is_empty() {
                ui.add_space(8.0);
                change_list(app, ui);
            }
        },
    );
}

/// The agent's viewport: what it said, and the diff of whichever change is
/// selected — at a size a diff can actually be read at.
pub fn agent_viewport(app: &mut App, ui: &mut egui::Ui) {
    if app.repo.is_none() {
        return;
    }
    if app.coding.summary.trim().is_empty() && app.coding.edits.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(if app.coding.running {
                    "Working…"
                } else {
                    "Give the agent a task in the panel on the left"
                })
                .color(theme::fg_dim()),
            );
        });
        return;
    }

    if !app.coding.summary.trim().is_empty() {
        let summary = app.coding.summary.clone();
        ScrollArea::vertical()
            .max_height(if app.coding.edits.is_empty() {
                ui.available_height()
            } else {
                (ui.available_height() * 0.3).clamp(80.0, 260.0)
            })
            .id_salt("agent-summary-view")
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(900.0));
                super::markdown::render(ui, &summary);
            });
    }

    if app.coding.edits.is_empty() {
        return;
    }
    ui.separator();
    apply_bar(app, ui);
    ui.separator();

    match app.coding.selected.and_then(|i| app.coding.edits.get(i)) {
        Some(proposed) => {
            let edit = proposed.edit.clone();
            super::dialogs::proposal_diff(ui, &edit, "agent-viewport-diff");
        }
        None => {
            ui.label(RichText::new("Select a change on the left.").color(theme::fg_dim()));
        }
    }
}

/// The task box and the controls that decide how the run behaves.
fn task_panel(app: &mut App, ui: &mut egui::Ui) {
    let busy = app.coding.running;
    ui.horizontal(|ui| {
        ui.label(theme::overline("TASK"));
        super::views::ai_model_picker(app, ui, AiTarget::Coding);

        let mut iterate = app.coding.iterate;
        let toggle = ui
            .checkbox(&mut iterate, "Let it iterate")
            .on_hover_text(
                "The agent writes to your working tree as it goes and can run this \
                 repository's own checks, so it can compile and test its work. You still \
                 review every change at the end and can revert any of it.\n\nWithout \
                 this, nothing is written until you accept it — but the agent cannot \
                 build or test what it wrote.",
            );
        if toggle.changed() {
            app.coding.iterate = iterate;
        }
        if app.coding.iterate {
            let checks = app.local_ci.jobs.len();
            let note = if checks == 0 {
                "no checks configured".to_string()
            } else {
                format!("{checks} check(s) available")
            };
            ui.label(RichText::new(note).small().color(theme::fg_dim()));
        }
    });

    ui.add_enabled(
        !busy,
        egui::TextEdit::multiline(&mut app.coding.task)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text(
                "What should it do? e.g. \"add a --json flag to devdock status and cover it \
                 with a test\"",
            ),
    );

    ui.horizontal(|ui| {
        let ready = !busy && !app.coding.task.trim().is_empty();
        if busy {
            ui.add(egui::Spinner::new().size(14.0));
            ui.label(RichText::new("working…").italics().weak());
        } else if ui
            .add_enabled(ready, egui::Button::new("Run").fill(theme::ember()))
            .on_hover_text("Send the task to the selected model")
            .clicked()
        {
            app.start_coding_agent();
        }

        // Starting a new task while changes are unreviewed would mix two
        // runs' edits together, so say so rather than silently merging them.
        if app.coding.awaiting_review() && !busy {
            ui.label(
                RichText::new("review the changes below first")
                    .small()
                    .color(theme::warn()),
            );
        }
        if !app.coding.history.is_empty()
            && !busy
            && ui
                .button("New session")
                .on_hover_text("Forget the conversation so far")
                .clicked()
        {
            app.coding.history.clear();
            app.coding.summary.clear();
            app.coding.log.clear();
        }
    });
}

/// Earlier tasks in this session.
fn transcript(app: &mut App, ui: &mut egui::Ui) {
    if app.coding.history.is_empty() {
        return;
    }
    for (i, exchange) in app.coding.history.iter().enumerate() {
        egui::CollapsingHeader::new(
            RichText::new(format!(
                "{}. {} — {} file(s)",
                i + 1,
                first_line(&exchange.task),
                exchange.changed
            ))
            .small(),
        )
        .id_salt(("agent-exchange", i))
        .default_open(false)
        .show(ui, |ui| {
            ui.label(RichText::new(&exchange.task).small().color(theme::fg_dim()));
            ui.separator();
            super::markdown::render(ui, &exchange.summary);
        });
    }
    ui.separator();
}

/// The agent's plan, ticked off as it goes.
///
/// This is the sidebar's main job while a run is going: a checklist the
/// model keeps up to date is a far better answer to "what is it doing" than
/// a scrolling log of tool calls.
fn plan(app: &mut App, ui: &mut egui::Ui) {
    let steps = app.coding.plan.clone();
    if steps.is_empty() {
        if app.coding.running {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("working out what to do…").small().color(theme::fg_dim()));
            });
        }
        return;
    }

    let done = steps.iter().filter(|s| s.done).count();
    ui.add_space(4.0);
    ui.label(theme::overline(&format!("PLAN — {done}/{} DONE", steps.len())));
    for step in &steps {
        ui.horizontal_top(|ui| {
            let (mark, color) = if step.done {
                ("✔", theme::add())
            } else if app.coding.running {
                ("○", theme::warn())
            } else {
                ("○", theme::fg_dim())
            };
            ui.label(RichText::new(mark).color(color).monospace());
            let text = RichText::new(&step.text).small();
            ui.add(
                egui::Label::new(if step.done {
                    text.color(theme::fg_dim()).strikethrough()
                } else {
                    text.color(theme::fg())
                })
                .wrap(),
            );
        });
    }
    ui.add_space(4.0);
}

/// What the model is doing right now.
fn activity(app: &mut App, ui: &mut egui::Ui) {
    let lines = app.coding.log.clone();
    egui::CollapsingHeader::new(format!("Activity ({} steps)", lines.len()))
        .id_salt("agent-activity")
        .default_open(app.coding.running)
        .show(ui, |ui| {
            ScrollArea::vertical()
                .max_height(220.0)
                .stick_to_bottom(true)
                .id_salt("agent-activity-log")
                .show(ui, |ui| {
                    for line in &lines {
                        ui.label(RichText::new(line).small().monospace().color(theme::fg_dim()));
                    }
                });
        });
}

/// The changes waiting on the user: the list, in the sidebar.
fn change_list(app: &mut App, ui: &mut egui::Ui) {
    let live = app.coding.live;
    ui.label(theme::overline(if live { "CHANGES ON DISK" } else { "PROPOSED CHANGES" }));
    ui.label(
        RichText::new(if live {
            "These are already in your working tree — the agent needed them there to build \
             and test. Tick anything you do not want and revert it."
        } else {
            "Nothing has been written. Tick what you want and apply it."
        })
        .small()
        .color(if live { theme::warn() } else { theme::fg_dim() }),
    );
    if app.coding.truncated {
        ui.label(
            RichText::new(
                "The model ran out of budget and stopped early — read these with extra care.",
            )
            .small()
            .color(theme::danger()),
        );
    }

    let mut select = None;
    for (i, proposed) in app.coding.edits.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add_enabled(!proposed.applied, egui::Checkbox::new(&mut proposed.accepted, ""));
            let (added, removed) = proposed.edit.line_delta();
            let label = format!(
                "{}{}  +{added} -{removed}",
                proposed.edit.path,
                if proposed.edit.is_new() { "  (new file)" } else { "" }
            );
            let color = if proposed.applied { theme::add() } else { theme::fg() };
            if ui
                .selectable_label(app.coding.selected == Some(i), RichText::new(label).color(color))
                .clicked()
            {
                select = Some(i);
            }
            if proposed.applied {
                ui.label(
                    RichText::new(if live { "kept" } else { "applied" })
                        .small()
                        .color(theme::add()),
                );
            }
        });
    }
    if let Some(i) = select {
        app.coding.selected = Some(i);
    }

}

/// Accept, revert, and the rest — above the diff they act on.
fn apply_bar(app: &mut App, ui: &mut egui::Ui) {
    let live = app.coding.live;
    ui.horizontal(|ui| {
        let pending = app.coding.pending_count();
        if live {
            if ui
                .add_enabled(
                    pending > 0,
                    egui::Button::new(format!("Revert {pending} selected")).fill(theme::danger()),
                )
                .on_hover_text("Restores the file exactly as it was before this run")
                .clicked()
            {
                app.revert_coding_edits();
            }
            if ui
                .add_enabled(
                    app.coding.awaiting_review(),
                    egui::Button::new("Keep everything").fill(theme::ember()),
                )
                .on_hover_text("Leaves every change in place and clears this list")
                .clicked()
            {
                app.keep_coding_edits();
            }
        } else if ui
            .add_enabled(
                pending > 0,
                egui::Button::new(format!("Apply {pending} selected")).fill(theme::ember()),
            )
            .on_hover_text("Writes only the ticked files")
            .clicked()
        {
            app.apply_coding_edits();
        }

        if ui.button("Select all").clicked() {
            for proposed in &mut app.coding.edits {
                if !proposed.applied {
                    proposed.accepted = true;
                }
            }
        }
        if ui.button("Select none").clicked() {
            for proposed in &mut app.coding.edits {
                proposed.accepted = false;
            }
        }
        if !live
            && ui
                .button("Discard")
                .on_hover_text("Throws away every unapplied proposal")
                .clicked()
        {
            app.coding.edits.retain(|e| e.applied);
            app.coding.selected = None;
        }
        if ui
            .button("Open in editor")
            .on_hover_text("Opens the selected file in the editor tab")
            .clicked()
        {
            app.open_selected_coding_edit();
        }
    });
}

fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    if line.chars().count() > 70 {
        format!("{}…", line.chars().take(70).collect::<String>())
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PendingEdit;

    fn edit(path: &str, accepted: bool, applied: bool) -> ProposedEdit {
        ProposedEdit {
            edit: PendingEdit {
                path: path.into(),
                before: Some("a\n".into()),
                after: "b\n".into(),
            },
            accepted,
            applied,
            unresolved: false,
        }
    }

    #[test]
    fn pending_counts_only_ticked_and_unapplied_changes() {
        let state = CodingState {
            edits: vec![
                edit("a.rs", true, false),
                edit("b.rs", false, false),
                edit("c.rs", true, true),
            ],
            ..Default::default()
        };
        assert_eq!(state.pending_count(), 1);
        assert!(state.awaiting_review());
    }

    #[test]
    fn a_fully_applied_run_is_no_longer_awaiting_review() {
        let state = CodingState {
            edits: vec![edit("a.rs", true, true)],
            ..Default::default()
        };
        assert_eq!(state.pending_count(), 0);
        assert!(!state.awaiting_review());
    }

    #[test]
    fn the_transcript_becomes_the_harness_history() {
        let state = CodingState {
            history: vec![Exchange {
                task: "add a flag".into(),
                summary: "done".into(),
                changed: 2,
            }],
            ..Default::default()
        };
        let turns = state.turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].task, "add a flag");
        assert_eq!(turns[0].summary, "done");
    }

    #[test]
    fn long_task_lines_are_shortened_for_the_header() {
        let long = "x".repeat(200);
        assert!(first_line(&long).ends_with('…'));
        assert_eq!(first_line("one\ntwo"), "one");
    }
}
