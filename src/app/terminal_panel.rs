//! The terminal panel: shells at the bottom of the window.
//!
//! Tabs across the top, the screen below, and the keyboard routed to the
//! shell whenever the panel has focus. The panel is global rather than
//! belonging to a tab: a build running in the terminal is not something you
//! want to lose by looking at the diff.

use super::{theme, App};
use crate::terminal::vt::{Color, Style};
use egui::{Color32, RichText};

/// One shell.
pub struct Session {
    pub pty: crate::terminal::pty::Pty,
    pub title: String,
    /// Set once the shell has exited, so the tab can say so.
    pub finished: bool,
}

/// Every terminal, and how the panel is showing them.
#[derive(Default)]
pub struct TerminalState {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub open: bool,
    /// Panel height in points, dragged by the user.
    pub height: f32,
    /// The last size sent to the shell, so a resize is only sent on change.
    last_size: (u16, u16),
}

impl TerminalState {
    pub fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active)
    }

    /// How many shells are still running, for the toggle button.
    pub fn running(&self) -> usize {
        self.sessions.iter().filter(|s| s.pty.alive()).count()
    }
}

/// Maps a terminal colour onto the app's palette, so the terminal looks
/// like part of the app rather than like a 1998 xterm.
fn color(value: Color, bright: bool) -> Color32 {
    let base = match value {
        Color::Black => Color32::from_rgb(0x2b, 0x30, 0x3b),
        Color::Red => theme::danger(),
        Color::Green => theme::add(),
        Color::Yellow => theme::warn(),
        Color::Blue => Color32::from_rgb(0x6f, 0x9b, 0xf0),
        Color::Magenta => Color32::from_rgb(0xc0, 0x8c, 0xe8),
        Color::Cyan => theme::teal(),
        Color::White => theme::fg(),
        Color::Bright(n) => return color(Color::from_index(n), true),
        // The 256-colour cube and true colour are used verbatim; there is
        // no palette to map them onto.
        Color::Indexed(n) => return indexed(n),
        Color::Rgb(r, g, b) => return Color32::from_rgb(r, g, b),
    };
    if bright {
        base.gamma_multiply(1.3)
    } else {
        base
    }
}

/// The xterm 256-colour palette, computed rather than tabulated.
fn indexed(n: u8) -> Color32 {
    match n {
        0..=7 => color(Color::from_index(n), false),
        8..=15 => color(Color::from_index(n - 8), true),
        16..=231 => {
            let n = n - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(step(n / 36), step((n / 6) % 6), step(n % 6))
        }
        _ => {
            let level = 8 + (n - 232) * 10;
            Color32::from_rgb(level, level, level)
        }
    }
}

/// Foreground and background for a cell, honouring inverse and dim.
fn cell_colors(style: Style) -> (Color32, Option<Color32>) {
    let mut fg = style.fg.map(|c| color(c, style.bold)).unwrap_or(theme::fg());
    let mut bg = style.bg.map(|c| color(c, false));
    if style.inverse {
        let previous = bg.unwrap_or(theme::bg());
        bg = Some(fg);
        fg = previous;
    }
    if style.dim {
        fg = fg.gamma_multiply(0.6);
    }
    (fg, bg)
}

/// Draws the panel, when it is open.
pub fn panel(app: &mut App, ctx: &egui::Context) {
    if !app.terminal.open {
        return;
    }
    if app.terminal.height <= 0.0 {
        app.terminal.height = 260.0;
    }

    egui::TopBottomPanel::bottom("terminal")
        .resizable(true)
        .default_height(app.terminal.height)
        .height_range(120.0..=(ctx.screen_rect().height() * 0.8))
        .frame(
            egui::Frame::new()
                .fill(theme::bg())
                .inner_margin(egui::Margin::symmetric(8, 6)),
        )
        .show(ctx, |ui| {
            tabs(app, ui);
            ui.separator();
            screen(app, ui);
        });
}

/// The row of shells, plus new and close.
fn tabs(app: &mut App, ui: &mut egui::Ui) {
    let mut close: Option<usize> = None;
    let mut activate: Option<usize> = None;

    ui.horizontal_wrapped(|ui| {
        for (i, session) in app.terminal.sessions.iter().enumerate() {
            let label = if session.pty.alive() {
                session.title.clone()
            } else {
                format!("{} (exited)", session.title)
            };
            let color = if session.pty.alive() { theme::fg() } else { theme::fg_dim() };
            if ui
                .selectable_label(app.terminal.active == i, RichText::new(label).color(color))
                .clicked()
            {
                activate = Some(i);
            }
        }
        if ui.small_button("+").on_hover_text("New terminal").clicked() {
            app.terminal_open(true);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Hide the panel (Ctrl+`)").clicked() {
                app.terminal.open = false;
            }
            if !app.terminal.sessions.is_empty()
                && ui.small_button("Close").on_hover_text("End this shell").clicked()
            {
                close = Some(app.terminal.active);
            }
            if let Some(session) = app.terminal.active_session() {
                if session.pty.alive()
                    && ui
                        .small_button("^C")
                        .on_hover_text("Interrupt the running program")
                        .clicked()
                {
                    session.pty.signal(libc::SIGINT);
                }
            }
        });
    });

    if let Some(i) = activate {
        app.terminal.active = i;
    }
    if let Some(i) = close {
        app.terminal.sessions.remove(i);
        app.terminal.active = app.terminal.active.min(app.terminal.sessions.len().saturating_sub(1));
    }
}

/// The screen itself, and the keyboard that drives it.
fn screen(app: &mut App, ui: &mut egui::Ui) {
    if app.terminal.sessions.is_empty() {
        ui.add_space(8.0);
        ui.label(
            RichText::new(if crate::terminal::SUPPORTED {
                "No terminal running. Press + to start one."
            } else {
                "The integrated terminal needs a pty, which this platform does not provide."
            })
            .color(theme::fg_dim()),
        );
        return;
    }
    let index = app.terminal.active.min(app.terminal.sessions.len() - 1);

    let font = egui::FontId::monospace(12.5);
    let (char_width, row_height) = ui.fonts(|f| {
        (f.glyph_width(&font, 'M'), f.row_height(&font))
    });

    // Tell the shell how big its window is, whenever that changes.
    let cols = ((ui.available_width() - 12.0) / char_width).floor().max(20.0) as u16;
    let rows = ((ui.available_height() - 4.0) / row_height).floor().max(4.0) as u16;
    if (cols, rows) != app.terminal.last_size {
        app.terminal.last_size = (cols, rows);
        app.terminal.sessions[index].pty.resize(cols, rows);
    }

    // Focus: clicking the screen sends the keyboard to the shell.
    let id = egui::Id::new(("terminal-screen", index));
    let focused = ui.memory(|m| m.has_focus(id));
    if focused {
        let bytes = read_keys(ui);
        if !bytes.is_empty() {
            app.terminal.sessions[index].pty.write(&bytes);
        }
    }

    #[allow(clippy::type_complexity)]
    let (lines, cursor, alternate): (Vec<Vec<(char, Style)>>, (usize, usize), bool) = {
        let session = &app.terminal.sessions[index];
        let Ok(screen) = session.pty.screen.lock() else { return };
        // Only the visible tail is drawn; scrollback is reachable by
        // scrolling, and drawing five thousand rows every frame is not.
        let all = screen.lines();
        let text: Vec<Vec<(char, Style)>> = all
            .iter()
            .map(|row| row.iter().map(|c| (c.ch, c.style)).collect())
            .collect();
        (text, screen.cursor_position(), screen.alternate_screen())
    };

    // Hints before the screen, never after it. Anything drawn below a
    // scroll area that fills the panel adds its own height to the content,
    // and a resizable panel grows to fit its content — one label per frame,
    // until it takes over the window.
    if !focused {
        ui.label(
            RichText::new("click the terminal to type in it")
                .small()
                .color(theme::fg_dim()),
        );
    }
    if alternate {
        ui.label(
            RichText::new(
                "A full-screen program is running; it is drawn approximately. \
                 Press q or ^C to leave it.",
            )
            .small()
            .color(theme::warn()),
        );
    }

    let available = ui.available_height();
    let response = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(available)
        .stick_to_bottom(true)
        .id_salt(("terminal-scroll", index))
        .show_rows(ui, row_height, lines.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                let row = &lines[i];
                let mut job = egui::text::LayoutJob::default();
                let mut run = String::new();
                let mut run_style: Option<Style> = None;
                let trimmed = row
                    .iter()
                    .rposition(|(ch, style)| *ch != ' ' || style.bg.is_some())
                    .map(|last| &row[..=last])
                    .unwrap_or(&[]);

                for (ch, style) in trimmed {
                    // Runs of one style become one section: a job with a
                    // section per character is unusably slow.
                    if run_style != Some(*style) && !run.is_empty() {
                        append(&mut job, &run, run_style.unwrap_or_default(), &font);
                        run.clear();
                    }
                    run_style = Some(*style);
                    run.push(*ch);
                }
                if !run.is_empty() {
                    append(&mut job, &run, run_style.unwrap_or_default(), &font);
                }
                // The cursor, drawn as a block on its row.
                if i == cursor.0 && !alternate {
                    let column = cursor.1;
                    if column >= trimmed.len() {
                        append(
                            &mut job,
                            &" ".repeat(column - trimmed.len() + 1),
                            Style { inverse: true, ..Default::default() },
                            &font,
                        );
                    }
                }
                ui.label(job);
            }
        })
        .inner_rect;

    // Clicking anywhere in the screen focuses it.
    let click = ui.interact(response, id, egui::Sense::click());
    if click.clicked() {
        ui.memory_mut(|m| m.request_focus(id));
    }
}

fn append(job: &mut egui::text::LayoutJob, text: &str, style: Style, font: &egui::FontId) {
    let (fg, bg) = cell_colors(style);
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: fg,
            background: bg.unwrap_or(Color32::TRANSPARENT),
            italics: style.italic,
            underline: if style.underline {
                egui::Stroke::new(1.0_f32, fg)
            } else {
                egui::Stroke::NONE
            },
            ..Default::default()
        },
    );
}

/// Translates this frame's key events into what a terminal would send.
fn read_keys(ui: &egui::Ui) -> Vec<u8> {
    use egui::{Event, Key};
    let mut out = Vec::new();
    ui.input(|input| {
        for event in &input.events {
            match event {
                Event::Text(text) => out.extend_from_slice(text.as_bytes()),
                Event::Key { key, pressed: true, modifiers, .. } => {
                    // Control characters first: ^C, ^D, ^Z and friends are
                    // the whole point of typing into a terminal.
                    if modifiers.ctrl || modifiers.mac_cmd && false {
                        if let Some(letter) = key.name().chars().next() {
                            let upper = letter.to_ascii_uppercase();
                            if upper.is_ascii_uppercase() {
                                out.push(upper as u8 - b'A' + 1);
                                continue;
                            }
                        }
                    }
                    match key {
                        Key::Enter => out.push(b'\r'),
                        Key::Tab => out.push(b'\t'),
                        Key::Backspace => out.push(0x7f),
                        Key::Escape => out.push(0x1b),
                        Key::ArrowUp => out.extend_from_slice(b"\x1b[A"),
                        Key::ArrowDown => out.extend_from_slice(b"\x1b[B"),
                        Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
                        Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
                        Key::Home => out.extend_from_slice(b"\x1b[H"),
                        Key::End => out.extend_from_slice(b"\x1b[F"),
                        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
                        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
                        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });
    out
}
