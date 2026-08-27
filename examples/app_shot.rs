//! Screenshots the app's own layout, so it can be looked at rather than
//! reasoned about.
//!
//! `cargo run --example app_shot -- <repo> <tab> <out.rgba> [width] [height]`
//! where tab is one of: changes, history, checks, editor, agent.

use eframe::egui;
use git_manage::app::{views, App, Tab};

struct Shot {
    app: App,
    out: String,
    frame: u32,
    /// Opened after the repository has finished loading: opening it sooner
    /// is undone, because a repository change clears the editor.
    open: Option<String>,
    shoot_at: u32,
}

impl eframe::App for Shot {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.app.pump_for_tools();
        if self.frame == 5 {
            if let Some(file) = self.open.take() {
                self.app.editor_open(std::path::Path::new(&file), None);
            }
            if std::env::var("TERMINAL").is_ok() {
                self.app.terminal_open(false);
                if let Some(command) = std::env::var("TERMINAL_CMD").ok() {
                    self.app.terminal_run(&command);
                }
            }
        }

        views::sidebar(&mut self.app, ctx);
        // Bottom panel before the central one, as the app does it.
        #[cfg(unix)]
        git_manage::app::terminal_panel::panel(&mut self.app, ctx);
        views::diff_panel(&mut self.app, ctx);

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
            Ok(Box::new(Shot { app, out, frame: 0, open: open.clone(), shoot_at }))
        }),
    )
}
