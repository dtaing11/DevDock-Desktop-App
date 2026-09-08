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
    /// A change the sidebar asked the viewport to scroll to. The list is
    /// still the way to navigate; it just moves the viewport now rather than
    /// swapping what is in it.
    pub scroll_to: Option<usize>,
    /// When the current run started, for the clock on the harness strip.
    pub started: Option<std::time::Instant>,
    /// How long the last finished run took, kept so the strip can say so
    /// after the fact rather than resetting to nothing.
    pub took: Option<std::time::Duration>,
    /// Model turns the last run took, and what it cost in tokens when the
    /// provider reports that.
    pub turns: usize,
    pub usage: Option<crate::agent::Usage>,
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

/// The agent's viewport: the harness at work, what it said, and every change
/// it made — at a size a diff can actually be read at.
///
/// All of the changes, not the selected one. A run that touches six files is
/// six diffs the user has to read before ticking anything, and making them
/// click through one at a time in a panel this size is asking them to skim.
/// The sidebar list stays, as a way to jump.
pub fn agent_viewport(app: &mut App, ui: &mut egui::Ui) {
    if app.repo.is_none() {
        return;
    }
    let idle = !app.coding.running
        && app.coding.summary.trim().is_empty()
        && app.coding.edits.is_empty();
    if idle {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Give the agent a task in the panel on the left")
                    .color(theme::fg_dim()),
            );
        });
        return;
    }

    harness(app, ui);

    // While it works and has nothing to show yet, the viewport is the run:
    // the plan as a pipeline, and the trace under it. The alternative is a
    // page of empty space next to a model that may or may not be alive.
    if app.coding.running && app.coding.edits.is_empty() {
        pipeline(app, ui);
        return;
    }

    let scroll_to = app.coding.scroll_to.take();
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_salt("agent-viewport-body")
        .show(ui, |ui| {
            if !app.coding.summary.trim().is_empty() {
                let summary = app.coding.summary.clone();
                ui.add_space(theme::UNIT * 2.0);
                egui::Frame::new()
                    .fill(theme::panel())
                    .stroke(egui::Stroke::new(1.0_f32, theme::border()))
                    .corner_radius(theme::RADIUS_MD as f32)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        super::markdown::render(ui, &summary);
                    });
            }

            if app.coding.edits.is_empty() {
                return;
            }
            ui.add_space(theme::UNIT * 3.0);
            apply_bar(app, ui);
            ui.add_space(theme::UNIT * 2.0);

            for i in 0..app.coding.edits.len() {
                let response = change_card(app, ui, i);
                if scroll_to == Some(i) {
                    response.scroll_to_me(Some(egui::Align::TOP));
                }
                ui.add_space(theme::UNIT * 3.0);
            }
        });
}

/// One change: its header, its checkbox, and its whole diff.
fn change_card(app: &mut App, ui: &mut egui::Ui, index: usize) -> egui::Response {
    let Some(proposed) = app.coding.edits.get(index) else {
        return ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover());
    };
    let path = proposed.edit.path.clone();
    let edit = proposed.edit.clone();
    let (added, removed) = proposed.edit.line_delta();
    let is_new = proposed.edit.is_new();
    let applied = proposed.applied;
    let selected = app.coding.selected == Some(index);
    let live = app.coding.live;

    // A card that has just appeared eases in, so a change arriving mid-run
    // is something you notice rather than something that was suddenly there.
    let age = ui.ctx().animate_bool_with_time(
        egui::Id::new(("agent-card", &path)),
        true,
        0.35,
    );

    let frame = egui::Frame::new()
        .fill(theme::panel())
        .stroke(egui::Stroke::new(
            1.0_f32,
            if selected { theme::ember() } else { theme::border() },
        ))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(12, 10));

    let inner = frame.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            let mut accepted = app.coding.edits[index].accepted;
            if ui.add_enabled(!applied, egui::Checkbox::new(&mut accepted, "")).changed() {
                app.coding.edits[index].accepted = accepted;
            }
            ui.label(
                RichText::new(&path)
                    .font(theme::semibold(theme::TEXT))
                    .color(if applied { theme::add() } else { theme::fg() }),
            );
            if is_new {
                ui.label(RichText::new("new file").size(theme::SMALL).color(theme::teal()));
            }
            ui.label(RichText::new(format!("+{added}")).size(theme::SMALL).color(theme::add()));
            ui.label(RichText::new(format!("-{removed}")).size(theme::SMALL).color(theme::del()));
            if applied {
                ui.label(
                    RichText::new(if live { "kept" } else { "applied" })
                        .size(theme::SMALL)
                        .color(theme::add()),
                );
            }
        });
        ui.add_space(theme::UNIT);
        // No path above the lines: the card's header already said it, and
        // no inner scroll, because the viewport is the scroll.
        super::dialogs::proposal_diff_body(
            ui,
            &edit,
            None,
            &format!("agent-change-{index}"),
        );
    });

    // The ease-in: a card at rest is fully opaque, so this only shows while
    // one is arriving.
    if age < 1.0 {
        ui.painter().rect_filled(
            inner.response.rect,
            theme::RADIUS_MD as f32,
            theme::bg().gamma_multiply(1.0 - age),
        );
        ui.ctx().request_repaint();
    }
    if inner.response.clicked() {
        app.coding.selected = Some(index);
    }
    inner.response
}

/// The harness at work: what it is doing, how far in, and for how long.
///
/// A model that is thinking looks identical to one that has hung, so this is
/// deliberately in motion while a run is live — the pulse and the sweep are
/// the difference between "working" and "stopped", and neither can be told
/// from a static screenshot.
fn harness(app: &mut App, ui: &mut egui::Ui) {
    let running = app.coding.running;
    let steps = app.coding.plan.clone();
    let done = steps.iter().filter(|s| s.done).count();
    let log = app.coding.log.clone();
    let time = ui.input(|i| i.time) as f32;

    egui::Frame::new()
        .fill(theme::panel2())
        .stroke(egui::Stroke::new(1.0_f32, theme::border()))
        .corner_radius(theme::RADIUS_MD as f32)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                status_dot(ui, running, time);
                ui.add_space(theme::UNIT);
                let (text, color) = match (running, app.coding.error.is_some()) {
                    (true, _) => (current_action(&log), theme::fg()),
                    (false, true) => ("Stopped".to_string(), theme::danger()),
                    (false, false) => ("Finished".to_string(), theme::add()),
                };
                ui.label(RichText::new(text).font(theme::semibold(theme::TEXT)).color(color));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(elapsed) = elapsed(app) {
                        ui.label(
                            RichText::new(elapsed)
                                .monospace()
                                .size(theme::SMALL)
                                .color(theme::fg_dim()),
                        );
                    }
                    if let Some(cost) = cost_line(app) {
                        ui.label(
                            RichText::new(cost).size(theme::SMALL).color(theme::fg_dim()),
                        )
                        .on_hover_text(
                            "Turns the model took, and tokens: what it read fresh, what \
                             came from the prompt cache at a fraction of the price, and \
                             what it wrote.",
                        );
                    }
                    if !log.is_empty() {
                        ui.label(
                            RichText::new(format!("{} steps", log.len()))
                                .size(theme::SMALL)
                                .color(theme::fg_dim()),
                        );
                    }
                    if !app.coding.edits.is_empty() {
                        ui.label(
                            RichText::new(format!("{} file(s)", app.coding.edits.len()))
                                .size(theme::SMALL)
                                .color(theme::fg_dim()),
                        );
                    }
                });
            });

            ui.add_space(theme::UNIT * 1.5);
            progress(ui, running, done, steps.len(), time);

            // The last few actions, newest last and brightest. Enough to see
            // the shape of what it is doing without the whole log, which is
            // in the sidebar for when it matters.
            if !log.is_empty() {
                ui.add_space(theme::UNIT * 1.5);
                let tail: Vec<&String> = log.iter().rev().take(3).collect();
                for (i, line) in tail.into_iter().rev().enumerate() {
                    let fade = match i {
                        0 if log.len() > 2 => 0.45,
                        1 if log.len() > 1 => 0.7,
                        _ => 1.0,
                    };
                    let color = if line.starts_with('!') {
                        theme::danger()
                    } else {
                        theme::fg_dim()
                    };
                    ui.label(
                        RichText::new(line)
                            .monospace()
                            .size(theme::SMALL)
                            .color(color.gamma_multiply(fade)),
                    );
                }
            }
        });

    if running {
        // Motion needs frames; without this the strip animates only when
        // something else happens to repaint.
        ui.ctx().request_repaint();
    }
}

/// The plan as a pipeline, drawn at viewport size while the run is live.
///
/// The connector between the last finished step and the current one carries a
/// travelling dot. It is the one part of this screen that proves the harness
/// is still turning: a model thinking for ninety seconds and a model that has
/// died look exactly alike otherwise.
fn pipeline(app: &mut App, ui: &mut egui::Ui) {
    let steps = app.coding.plan.clone();
    let time = ui.input(|i| i.time) as f32;
    ui.add_space(theme::UNIT * 4.0);

    ScrollArea::vertical().auto_shrink([false, false]).id_salt("agent-pipeline").show(
        ui,
        |ui| {
            if steps.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(theme::UNIT * 6.0);
                    ui.label(
                        RichText::new("Working out what to do…").color(theme::fg_dim()),
                    );
                });
            }
            let active = steps.iter().position(|s| !s.done);
            for (i, step) in steps.iter().enumerate() {
                node(ui, &step.text, step.done, active == Some(i), time);
                if i + 1 < steps.len() {
                    // The dot travels down the line *into* the step being
                    // worked on, which is where the run actually is.
                    connector(ui, active == Some(i + 1), time);
                }
            }

            let log = app.coding.log.clone();
            if !log.is_empty() {
                ui.add_space(theme::UNIT * 5.0);
                ui.label(theme::overline("TRACE"));
                ui.add_space(theme::UNIT);
                for line in &log {
                    let color = if line.starts_with('!') {
                        theme::danger()
                    } else if line.starts_with('…') {
                        theme::fg_dim().gamma_multiply(0.8)
                    } else {
                        theme::fg_dim()
                    };
                    ui.label(
                        RichText::new(line).monospace().size(theme::SMALL).color(color),
                    );
                }
            }
        },
    );
}

/// One step of the pipeline: a marker and its text.
fn node(ui: &mut egui::Ui, text: &str, done: bool, active: bool, time: f32) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
        let centre = rect.center();
        let painter = ui.painter();
        if done {
            painter.circle_filled(centre, 6.0, theme::add().gamma_multiply(0.25));
            painter.circle_stroke(centre, 6.0, egui::Stroke::new(1.5_f32, theme::add()));
            painter.text(
                centre,
                egui::Align2::CENTER_CENTER,
                "✔",
                egui::FontId::proportional(9.0),
                theme::add(),
            );
        } else if active {
            // A ring that breathes outward, so the eye lands here first.
            let pulse = (time * 2.0).sin() * 0.5 + 0.5;
            painter.circle_filled(
                centre,
                6.0 + pulse * 5.0,
                theme::ember().gamma_multiply(0.22 * (1.0 - pulse)),
            );
            painter.circle_stroke(centre, 6.0, egui::Stroke::new(1.5_f32, theme::ember()));
            painter.circle_filled(centre, 2.5, theme::ember());
        } else {
            painter.circle_stroke(centre, 5.0, egui::Stroke::new(1.0_f32, theme::border()));
        }
        ui.add_space(theme::UNIT);
        let text = RichText::new(text).size(theme::TEXT);
        ui.add(
            egui::Label::new(if done {
                text.color(theme::fg_dim())
            } else if active {
                text.font(theme::semibold(theme::TEXT)).color(theme::fg())
            } else {
                text.color(theme::fg_dim().gamma_multiply(0.7))
            })
            .wrap(),
        );
    });
}

/// The line between two steps, with a dot travelling down the live one.
fn connector(ui: &mut egui::Ui, live: bool, time: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
    let x = rect.center().x;
    let (top, bottom) = (rect.top(), rect.bottom());
    ui.painter().line_segment(
        [egui::pos2(x, top), egui::pos2(x, bottom)],
        egui::Stroke::new(1.0_f32, theme::border()),
    );
    if live {
        let t = (time * 0.9).fract();
        let y = top + (bottom - top) * t;
        ui.painter().circle_filled(
            egui::pos2(x, y),
            2.0,
            theme::ember().gamma_multiply(1.0 - (t - 0.5).abs() * 1.2),
        );
    }
}

/// The state light: a steady dot at rest, a breathing one while working.
fn status_dot(ui: &mut egui::Ui, running: bool, time: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let centre = rect.center();
    let color = if running { theme::ember() } else { theme::add() };
    if running {
        // A halo that breathes once a second, and a core that stays put, so
        // the eye reads "alive" rather than "flickering".
        let pulse = (time * 2.4).sin() * 0.5 + 0.5;
        ui.painter().circle_filled(
            centre,
            3.0 + pulse * 4.0,
            color.gamma_multiply(0.28 * (1.0 - pulse * 0.6)),
        );
    }
    ui.painter().circle_filled(centre, 3.5, color);
}

/// Plan progress, or a sweep when there is no plan to measure against.
fn progress(ui: &mut egui::Ui, running: bool, done: usize, total: usize, time: f32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    let radius = 2.0;
    ui.painter().rect_filled(rect, radius, theme::border());

    if total > 0 {
        // Eased, so ticking off a step slides rather than jumps.
        let target = done as f32 / total as f32;
        let shown = ui.ctx().animate_value_with_time(
            egui::Id::new("agent-plan-progress"),
            target,
            0.4,
        );
        let mut filled = rect;
        filled.set_width(rect.width() * shown);
        ui.painter().rect_filled(filled, radius, theme::ember());
        return;
    }
    if !running {
        return;
    }
    // No plan yet: a segment sweeping the track says "working" without
    // claiming progress it cannot measure.
    let width = rect.width();
    let span = (width * 0.22).max(40.0);
    let travel = width + span;
    let x = (time * 0.6).fract() * travel - span;
    let mut sweep = rect;
    sweep.min.x = rect.min.x + x.max(0.0);
    sweep.max.x = (rect.min.x + x + span).min(rect.max.x);
    if sweep.max.x > sweep.min.x {
        ui.painter().rect_filled(sweep, radius, theme::ember().gamma_multiply(0.8));
    }
}

/// What the harness is doing right now, from its last step.
fn current_action(log: &[String]) -> String {
    let Some(last) = log.last() else {
        return "Starting…".to_string();
    };
    let text = last.trim_start_matches(['·', '!', '…', ' ']).trim();
    if text.is_empty() {
        "Working…".to_string()
    } else {
        let mut out = text.to_string();
        if let Some((first, _)) = out.split_once('\n') {
            out = first.to_string();
        }
        if out.chars().count() > 90 {
            out = out.chars().take(89).collect::<String>() + "…";
        }
        out
    }
}

/// The run clock: counting up while it works, the total once it is done.
fn elapsed(app: &App) -> Option<String> {
    let d = match (app.coding.started, app.coding.took) {
        (Some(started), _) => started.elapsed(),
        (None, Some(took)) => took,
        _ => return None,
    };
    let secs = d.as_secs();
    Some(format!("{}:{:02}", secs / 60, secs % 60))
}

/// Turns and tokens of the last run, once it is done.
fn cost_line(app: &App) -> Option<String> {
    if app.coding.running || app.coding.turns == 0 {
        return None;
    }
    let k = |n: u64| -> String {
        if n >= 10_000 {
            format!("{}k", n / 1000)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1000.0)
        } else {
            n.to_string()
        }
    };
    let mut line = format!("{} turn(s)", app.coding.turns);
    if let Some(u) = app.coding.usage {
        line.push_str(&format!(
            " · {} in, {} cached, {} out",
            k(u.input_tokens + u.cache_write_tokens),
            k(u.cache_read_tokens),
            k(u.output_tokens)
        ));
    }
    Some(line)
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
            // The viewport carries the live state; this only has to say the
            // run is still yours and offer no button that would fight it.
            ui.add(egui::Spinner::new().size(theme::SPINNER).color(theme::ember()));
            ui.label(RichText::new("working…").size(theme::TEXT).color(theme::fg_dim()));
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
                ui.add(egui::Spinner::new().size(theme::SPINNER));
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
        app.coding.scroll_to = Some(i);
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
