//! Screenshots the Markdown renderer, so its output can be looked at rather
//! than reasoned about.
//!
//! `cargo run --example md_shot -- <file.md> <out.rgba> [width]`
//!
//! Writes raw RGBA plus a `<out>.size` file with the dimensions. A window
//! flashes up for a few frames and closes itself.

use eframe::egui;
use git_manage::app::{markdown, theme};

struct Shot {
    md: String,
    out: String,
    frame: u32,
    scroll: f32,
}

impl eframe::App for Shot {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(12.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .vertical_scroll_offset(self.scroll)
                    .show(ui, |ui| {
                    let width = ui.available_width().min(900.0);
                    ui.set_max_width(width);
                    markdown::render(ui, &self.md);
                });
            });

        // Let the font atlas settle, then grab the frame and leave.
        if self.frame == 3 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        if self.frame > 3 {
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = shot {
                let bytes: Vec<u8> =
                    image.pixels.iter().flat_map(|p| p.to_array()).collect();
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
    let path = args.get(1).cloned().unwrap_or_else(|| "README.md".into());
    let out = args.get(2).cloned().unwrap_or_else(|| "/tmp/md.rgba".into());
    let width: f32 = args.get(3).and_then(|w| w.parse().ok()).unwrap_or(1100.0);
    let scroll: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let md = std::fs::read_to_string(&path).expect("readable markdown file");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([width, 1400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "md_shot",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Shot { md, out, frame: 0, scroll }))
        }),
    )
}
