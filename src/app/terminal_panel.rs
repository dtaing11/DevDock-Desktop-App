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
    /// The size last sent to this shell, so a resize is only sent on
    /// change — and is sent, for a shell that started at another size.
    size: (u16, u16),
    /// How far back the window is scrolled, in lines; zero is the live screen.
    offset: usize,
    /// Wheel movement not yet worth a whole line.
    wheel: f32,
    /// A selection, from where the drag started to where it is, as
    /// `(line, column)` in history-then-screen lines.
    selection: Option<((usize, usize), (usize, usize))>,
    /// Take the keyboard on the next frame: a terminal just opened is one
    /// somebody is about to type in.
    grab_focus: bool,
}

impl Session {
    pub fn new(pty: crate::terminal::pty::Pty, title: String) -> Self {
        Self { pty, title, finished: false, size: (0, 0), offset: 0, wheel: 0.0, selection: None, grab_focus: true }
    }
}

/// Every terminal, and how the panel is showing them.
#[derive(Default)]
pub struct TerminalState {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub open: bool,
    /// Panel height in points, dragged by the user.
    pub height: f32,
    /// The keyboard is the shell's: the app's own shortcuts stand aside.
    pub focused: bool,
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

/// The screen itself, and the keyboard and mouse that drive it.
fn screen(app: &mut App, ui: &mut egui::Ui) {
    if app.terminal.sessions.is_empty() {
        app.terminal.focused = false;
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
    // The width of a cell is the font's real advance, measured over a laid
    // out line: a run of text is placed once and advances by that, so a
    // cell width that is a fraction off drifts a long line into the next
    // run and past the edge.
    let (char_width, row_height) = ui.fonts(|f| {
        let probe = f.layout_no_wrap("M".repeat(64), font.clone(), Color32::WHITE);
        (probe.size().x / 64.0, f.row_height(&font))
    });

    // The whole of what is left is the screen: one rectangle, every cell
    // at a position computed from its row and column, so nothing a row
    // contains — a wide glyph, a fallback font — can move its neighbours.
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    let origin = rect.min + egui::vec2(4.0, 2.0);
    let cols = ((rect.width() - 8.0) / char_width).floor().max(20.0) as u16;
    let rows = ((rect.height() - 4.0) / row_height).floor().max(4.0) as u16;

    let session = &mut app.terminal.sessions[index];
    // Tell the shell how big its window is, whenever that changes — per
    // shell: a second one starts at its own size, not the first one's.
    if (cols, rows) != session.size {
        session.size = (cols, rows);
        session.pty.resize(cols, rows);
    }

    // Focus: a click takes the keyboard, and so does a terminal just opened.
    let id = response.id;
    if response.clicked() || response.drag_started() || std::mem::take(&mut session.grab_focus) {
        ui.memory_mut(|m| m.request_focus(id));
    }
    let focused = ui.memory(|m| m.has_focus(id));
    app.terminal.focused = focused;
    if focused {
        // Tab, the arrows and Escape are the shell's. Left to egui they
        // move the focus to the next widget, or drop it.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        });
    }

    let (modes, history) = match session.pty.screen.lock() {
        Ok(mut screen) => (Modes { application_cursor: screen.application_cursor(), bracketed_paste: screen.bracketed_paste(), alternate: screen.alternate_screen(), mouse: screen.mouse_reporting() }, screen.history()),
        Err(_) => return,
    };

    // The wheel: through the history, or — for a pager on the alternate
    // screen, which has none — as the arrow keys it would otherwise need.
    if response.hovered() {
        session.wheel += ui.input(|i| i.smooth_scroll_delta.y);
        let lines = (session.wheel / row_height).trunc();
        session.wheel -= lines * row_height;
        if lines != 0.0 {
            if modes.alternate {
                let key = if lines > 0.0 { egui::Key::ArrowUp } else { egui::Key::ArrowDown };
                if let Some(bytes) = key_bytes(key, egui::Modifiers::NONE, modes) {
                    for _ in 0..(lines.abs() as usize).min(10) {
                        session.pty.write(&bytes);
                    }
                }
            } else {
                session.offset = (session.offset as f32 + lines).clamp(0.0, history as f32) as usize;
            }
        }
    }

    // A drag selects, in lines of history-then-screen so the selection
    // stays on its text while the window scrolls.
    let cell_at = |pos: egui::Pos2, offset: usize| -> (usize, usize) {
        let row = ((pos.y - origin.y) / row_height).floor().clamp(0.0, rows as f32 - 1.0) as usize;
        let col = ((pos.x - origin.x) / char_width).floor().clamp(0.0, cols as f32 - 1.0) as usize;
        (history.saturating_sub(offset) + row, col)
    };
    if response.drag_started() {
        session.selection = response.interact_pointer_pos().map(|p| (cell_at(p, session.offset), cell_at(p, session.offset)));
    } else if response.dragged() {
        if let (Some((anchor, _)), Some(p)) = (session.selection, response.interact_pointer_pos()) {
            session.selection = Some((anchor, cell_at(p, session.offset)));
        }
    } else if response.clicked() {
        session.selection = None;
    }

    if focused {
        let input = read_input(ui, modes, session.selection.is_some());
        if input.copy {
            if let (Some((a, b)), Ok(screen)) = (session.selection, session.pty.screen.lock()) {
                ui.ctx().copy_text(screen.text_between(a, b));
            }
            session.selection = None;
        }
        if !input.bytes.is_empty() {
            session.pty.write(&input.bytes);
            // Typing is at the live screen, wherever the window was.
            session.offset = 0;
            session.selection = None;
        }
    }

    let view = match session.pty.screen.lock() {
        Ok(mut screen) => screen.view(session.offset),
        Err(_) => return,
    };
    session.offset = view.offset;

    let painter = ui.painter_at(rect);
    let cell_rect = |row: usize, col: usize, width: usize| {
        egui::Rect::from_min_size(origin + egui::vec2(col as f32 * char_width, row as f32 * row_height), egui::vec2(width as f32 * char_width, row_height))
    };
    let first_line = view.history - view.offset;
    let selected = |row: usize, col: usize| {
        session.selection.is_some_and(|(a, b)| {
            let (from, to) = if a <= b { (a, b) } else { (b, a) };
            let at = (first_line + row, col);
            from != to && at >= from && at <= to
        })
    };

    for (r, row) in view.rows.iter().enumerate() {
        // Backgrounds and the selection first, as rectangles by cell.
        for (c, cell) in row.iter().enumerate() {
            let (_, bg) = cell_colors(cell.style);
            if selected(r, c) {
                painter.rect_filled(cell_rect(r, c, 1), 0.0, theme::teal().linear_multiply(0.35));
            } else if let Some(bg) = bg {
                painter.rect_filled(cell_rect(r, c, 1), 0.0, bg);
            }
        }
        // Then the text: runs of one style, each placed at its own column.
        // A character outside ASCII is placed alone, since the font it
        // falls back to need not be as wide as a cell.
        let mut c = 0;
        while c < row.len() {
            let cell = &row[c];
            if cell.continuation || (cell.ch == ' ' && cell.more.is_empty() && !cell.style.underline) {
                c += 1;
                continue;
            }
            let start = c;
            let mut run = String::new();
            if cell.ch.is_ascii() && cell.more.is_empty() {
                while c < row.len() && row[c].style == cell.style && row[c].ch.is_ascii() && row[c].more.is_empty() && !row[c].continuation {
                    run.push(row[c].ch);
                    c += 1;
                }
            } else {
                run.push(cell.ch);
                run.push_str(&cell.more);
                c += 1;
            }
            let run = run.trim_end_matches(' ');
            if !run.is_empty() || cell.style.underline {
                paint_text(&painter, cell_rect(r, start, 1).min, run, cell.style, &font);
            }
        }
    }

    // The cursor: a block with its character drawn over it when the
    // keyboard is here, an outline when it is not.
    if let Some((r, c)) = view.cursor {
        let at = cell_rect(r, c, 1);
        if focused {
            painter.rect_filled(at, 1.0, theme::fg());
            let cell = &view.rows[r][c];
            if cell.ch != ' ' {
                let mut text = cell.ch.to_string();
                text.push_str(&cell.more);
                painter.text(at.min, egui::Align2::LEFT_TOP, text, font.clone(), theme::bg());
            }
        } else {
            painter.rect_stroke(at, 1.0, egui::Stroke::new(1.0_f32, theme::fg_dim()), egui::StrokeKind::Inside);
        }
    }

    // Where the window is, when it is not at the live screen.
    if view.offset > 0 {
        let label = format!(" {} lines back — type or scroll down to return ", view.offset);
        let galley = painter.layout_no_wrap(label, egui::FontId::proportional(11.0), theme::bg());
        let at = egui::pos2(rect.max.x - galley.size().x - 10.0, rect.min.y + 4.0);
        painter.rect_filled(egui::Rect::from_min_size(at, galley.size()).expand(2.0), 3.0, theme::warn());
        painter.galley(at, galley, theme::bg());
    }
    if !focused {
        let galley = painter.layout_no_wrap(" click to type ".into(), egui::FontId::proportional(11.0), theme::fg_dim());
        let at = egui::pos2(rect.max.x - galley.size().x - 10.0, rect.max.y - galley.size().y - 6.0);
        painter.rect_filled(egui::Rect::from_min_size(at, galley.size()).expand(2.0), 3.0, theme::panel2());
        painter.galley(at, galley, theme::fg_dim());
    }
}

fn paint_text(painter: &egui::Painter, at: egui::Pos2, text: &str, style: Style, font: &egui::FontId) {
    let (fg, _) = cell_colors(style);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: fg,
            italics: style.italic,
            underline: if style.underline { egui::Stroke::new(1.0_f32, fg) } else { egui::Stroke::NONE },
            ..Default::default()
        },
    );
    let galley = painter.layout_job(job);
    painter.galley(at, galley, fg);
}

/// What the program on the other end has asked of the keyboard and mouse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub alternate: bool,
    pub mouse: bool,
}

/// This frame's input, as what a terminal sends.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub bytes: Vec<u8>,
    /// Copy the selection to the clipboard.
    pub copy: bool,
}

fn read_input(ui: &egui::Ui, modes: Modes, has_selection: bool) -> Input {
    let events = ui.input(|i| i.events.clone());
    translate(&events, modes, has_selection)
}

/// Translates events into what a terminal would send. With a selection,
/// the copy shortcut copies; without one, Ctrl+C is the interrupt it
/// always was.
pub fn translate(events: &[egui::Event], modes: Modes, has_selection: bool) -> Input {
    use egui::Event;
    let mut input = Input::default();
    let copying = has_selection && events.iter().any(|e| matches!(e, Event::Copy));
    input.copy = copying;
    for event in events {
        match event {
            Event::Text(text) => input.bytes.extend_from_slice(text.as_bytes()),
            Event::Paste(text) => input.bytes.extend(paste_bytes(text, modes.bracketed_paste)),
            Event::Key { key, pressed: true, modifiers, .. } => {
                // Where Ctrl+C is also "copy", a selection wins.
                if copying && *key == egui::Key::C && modifiers.ctrl {
                    continue;
                }
                if let Some(bytes) = key_bytes(*key, *modifiers, modes) {
                    input.bytes.extend(bytes);
                }
            }
            _ => {}
        }
    }
    input
}

/// A paste: line ends as the Return key sends them, and marked as a paste
/// when the program asked, so a shell does not run it line by line.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    // A paste that contains the end marker could close the bracket early
    // and have the rest run as typed.
    let text = text.replace("\x1b[201~", "");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

/// What one key sends, xterm's way. `None` for a key that sends nothing
/// by itself — a letter arrives as text, not as a key.
pub fn key_bytes(key: egui::Key, modifiers: egui::Modifiers, modes: Modes) -> Option<Vec<u8>> {
    use egui::Key;
    let name = key.name();
    let letter = (name.len() == 1).then(|| name.as_bytes()[0]).filter(u8::is_ascii_alphabetic);

    // Control characters: ^C, ^D, ^Z, ^R and friends. Only for keys that
    // are letters or the punctuation that has a control code — Ctrl+Enter
    // is not ^E because "Enter" starts with an E.
    if modifiers.ctrl && !modifiers.alt {
        if let Some(letter) = letter {
            return Some(vec![letter.to_ascii_uppercase() - b'A' + 1]);
        }
        match key {
            Key::Space => return Some(vec![0]),
            Key::OpenBracket => return Some(vec![0x1b]),
            Key::Backslash => return Some(vec![0x1c]),
            Key::CloseBracket => return Some(vec![0x1d]),
            Key::Backspace => return Some(vec![0x17]), // delete the word before
            _ => {}
        }
    }
    // Alt+letter is Meta: ESC, then the letter. (On macOS, Option+letter
    // types a character instead, and that arrives as text.)
    if modifiers.alt && !modifiers.ctrl && !cfg!(target_os = "macos") {
        if let Some(letter) = letter {
            let letter = if modifiers.shift { letter.to_ascii_uppercase() } else { letter.to_ascii_lowercase() };
            return Some(vec![0x1b, letter]);
        }
    }
    // The line-editing a Mac's other terminals give: by word with Option,
    // to the ends with Command.
    if modifiers.mac_cmd {
        match key {
            Key::ArrowLeft => return Some(vec![0x01]),
            Key::ArrowRight => return Some(vec![0x05]),
            Key::Backspace => return Some(vec![0x15]),
            _ => {}
        }
    }
    if modifiers.alt && !modifiers.ctrl {
        match key {
            Key::ArrowLeft => return Some(b"\x1bb".to_vec()),
            Key::ArrowRight => return Some(b"\x1bf".to_vec()),
            Key::Backspace => return Some(vec![0x1b, 0x7f]),
            _ => {}
        }
    }

    // xterm's modifier parameter: 1, plus 1 for Shift, 2 for Alt, 4 for Ctrl.
    let modifier = 1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);
    let cursor = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if modes.application_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |code: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };
    Some(match key {
        Key::Enter => vec![b'\r'],
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => vec![b'\t'],
        Key::Backspace => vec![0x7f],
        Key::Escape => vec![0x1b],
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers};

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn control_keys_are_control_characters_and_only_those() {
        let plain = Modes::default();
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, plain), Some(vec![3]));
        assert_eq!(key_bytes(Key::R, Modifiers::CTRL, plain), Some(vec![18]));
        assert_eq!(key_bytes(Key::Space, Modifiers::CTRL, plain), Some(vec![0]));
        // The first letter of a key's name is not its control code.
        assert_eq!(key_bytes(Key::Enter, Modifiers::CTRL, plain), Some(vec![b'\r']));
        assert_eq!(key_bytes(Key::ArrowLeft, Modifiers::CTRL, plain), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(key_bytes(Key::Backspace, Modifiers::CTRL, plain), Some(vec![0x17]));
        // A letter by itself arrives as text, not as a key.
        assert_eq!(key_bytes(Key::A, Modifiers::NONE, plain), None);
    }

    #[test]
    fn arrows_follow_the_mode_the_program_set() {
        let plain = Modes::default();
        let app = Modes { application_cursor: true, ..plain };
        assert_eq!(key_bytes(Key::ArrowUp, Modifiers::NONE, plain), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, Modifiers::NONE, app), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, Modifiers::SHIFT, app), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(key_bytes(Key::Tab, Modifiers::SHIFT, plain), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_bytes(Key::Delete, Modifiers::NONE, plain), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key_bytes(Key::F5, Modifiers::NONE, plain), Some(b"\x1b[15~".to_vec()));
        // By word, the way a Mac's terminals do it.
        assert_eq!(key_bytes(Key::ArrowLeft, Modifiers::ALT, plain), Some(b"\x1bb".to_vec()));
        assert_eq!(key_bytes(Key::Backspace, Modifiers::ALT, plain), Some(vec![0x1b, 0x7f]));
    }

    #[test]
    fn a_paste_reaches_the_shell_marked_when_asked() {
        assert_eq!(paste_bytes("ls\necho hi\r\n", false), b"ls\recho hi\r");
        assert_eq!(paste_bytes("ls\n", true), b"\x1b[200~ls\r\x1b[201~");
        assert_eq!(paste_bytes("a\x1b[201~rm -rf", true), b"\x1b[200~arm -rf\x1b[201~", "the end marker cannot be smuggled in");

        let got = translate(&[Event::Paste("hi".into()), Event::Text("x".into()), key(Key::Enter, Modifiers::NONE)], Modes::default(), false);
        assert_eq!(got, Input { bytes: b"hix\r".to_vec(), copy: false });
    }

    #[test]
    fn copy_copies_a_selection_and_interrupts_without_one() {
        let events = [Event::Copy, key(Key::C, Modifiers::CTRL)];
        assert_eq!(translate(&events, Modes::default(), false), Input { bytes: vec![3], copy: false });
        assert_eq!(translate(&events, Modes::default(), true), Input { bytes: vec![], copy: true });
    }
}
