//! A terminal screen.
//!
//! The emulation is the `vt100` crate's: a complete parser and screen —
//! the alternate screen that `less`, `vim` and `git log` draw on, scroll
//! regions, wide characters, UTF-8 that arrives split across reads,
//! character-set selection, the modes a program sets for the keyboard. An
//! emulator written here covered what command *output* uses and nothing
//! a program that draws needs; every gap was a bug somebody met.
//!
//! This module is what the rest of the app sees of it: cells with the
//! app's own colour names, a window onto the history, the modes the
//! keyboard has to honour, and the answers a terminal owes a program
//! that asks where the cursor is.
//!
//! Everything here is pure: bytes in, screen out. The pty lives next door
//! in [`super`], and this can be tested without one.

/// One character cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// What follows `ch` in the same cell: combining marks, the rest of an
    /// emoji sequence. Empty for almost every cell.
    pub more: String,
    pub style: Style,
    /// Two columns wide; the next cell is its second half and is not drawn.
    pub wide: bool,
    pub continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', more: String::new(), style: Style::default(), wide: false, continuation: false }
    }
}

/// How a cell is drawn. Colours are the 8 ANSI names plus bright variants,
/// left for the UI to map onto the app's palette; a terminal that hardcodes
/// #00FF00 looks like a terminal from 1998.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Swap foreground and background when drawing.
    pub inverse: bool,
    pub dim: bool,
}

/// An ANSI colour, by name or by index/rgb for the extended forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Bright(u8),
    /// 256-colour palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// The named colour for an index 0-7, for callers mapping a palette.
    pub fn from_index(code: u8) -> Self {
        match code % 8 {
            0 => Self::Black,
            1 => Self::Red,
            2 => Self::Green,
            3 => Self::Yellow,
            4 => Self::Blue,
            5 => Self::Magenta,
            6 => Self::Cyan,
            _ => Self::White,
        }
    }

    fn from_vt(color: vt100::Color) -> Option<Self> {
        match color {
            vt100::Color::Default => None,
            vt100::Color::Idx(n @ 0..=7) => Some(Self::from_index(n)),
            vt100::Color::Idx(n @ 8..=15) => Some(Self::Bright(n - 8)),
            vt100::Color::Idx(n) => Some(Self::Indexed(n)),
            vt100::Color::Rgb(r, g, b) => Some(Self::Rgb(r, g, b)),
        }
    }
}

/// Cap on scrollback, so a `cargo build` on a big workspace cannot grow
/// without limit.
const MAX_SCROLLBACK: usize = 5_000;

/// What the parser hands back besides the screen: the title a program
/// set, and what the terminal must answer.
#[derive(Default)]
struct Events {
    title: Option<String>,
    replies: Vec<u8>,
}

impl vt100::Callbacks for Events {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    /// The questions a program asks its terminal and then waits on: where
    /// the cursor is (shells and prompts, to find out whether the last
    /// output ended its line), whether the terminal is there at all, what
    /// kind it is. Unanswered, the program waits out a timeout every time.
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _i2: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            // Primary device attributes: a VT220 with nothing special.
            (None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            _ => {}
        }
    }
}

/// The part of the screen and its history that fits the window, for drawing.
pub struct View {
    pub rows: Vec<Vec<Cell>>,
    /// The cursor's row and column in `rows`; `None` when the program hid
    /// it or the window is scrolled away from it.
    pub cursor: Option<(usize, usize)>,
    /// How far back the window is, in lines, after clamping.
    pub offset: usize,
    /// How many lines of history there are to scroll back through.
    pub history: usize,
}

/// A terminal screen: a grid, a cursor, and the lines that have scrolled off.
pub struct Screen {
    parser: vt100::Parser<Events>,
    pub cols: usize,
    pub rows: usize,
    /// Bumped on every change, so a UI can tell whether to repaint.
    pub epoch: u64,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            parser: vt100::Parser::new_with_callbacks(rows as u16, cols as u16, MAX_SCROLLBACK, Events::default()),
            cols,
            rows,
            epoch: 0,
        }
    }

    /// Feeds output from the program. What comes back is what the terminal
    /// owes the program in answer — a cursor position it asked for — to be
    /// written to it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        self.epoch += 1;
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    /// Rows and columns, keeping the content that fits.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // A shorter window keeps the line the cursor is on: what no longer
        // fits goes off the top into the history, as in any terminal. Left
        // to the grid alone, the bottom rows — the prompt — are the ones cut.
        let (cursor_row, _) = self.parser.screen().cursor_position();
        let over = (cursor_row as usize + 1).saturating_sub(rows);
        if over > 0 && !self.alternate_screen() {
            let scroll = format!("\x1b7\x1b[{};1H{}\x1b8\x1b[{over}A", self.rows, "\n".repeat(over));
            self.parser.process(scroll.as_bytes());
        }
        self.parser.screen_mut().set_size(rows as u16, cols as u16);
        self.cols = cols;
        self.rows = rows;
        self.epoch += 1;
    }

    /// The title the program set, if any.
    pub fn title(&self) -> Option<&str> {
        self.parser.callbacks().title.as_deref()
    }

    /// Whether a full-screen program is currently in control.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// The program asked for arrow keys in their application form.
    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    /// The program wants a paste marked as one, so it is not run line by
    /// line as though typed.
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// The program is listening for the mouse: the wheel is its to handle.
    pub fn mouse_reporting(&self) -> bool {
        self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
    }

    /// How many lines have scrolled off the top.
    pub fn history(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        screen.set_scrollback(0);
        history
    }

    /// The window `offset` lines back from the live screen.
    pub fn view(&mut self, offset: usize) -> View {
        let history = self.history();
        let offset = offset.min(history);
        let screen = self.parser.screen_mut();
        screen.set_scrollback(offset);
        let rows = (0..self.rows).map(|row| Self::row_of(screen, row as u16, self.cols)).collect();
        screen.set_scrollback(0);
        let cursor = (offset == 0 && !screen.hide_cursor()).then(|| {
            let (row, col) = screen.cursor_position();
            (row as usize, (col as usize).min(self.cols.saturating_sub(1)))
        });
        View { rows, cursor, offset, history }
    }

    fn row_of(screen: &vt100::Screen, row: u16, cols: usize) -> Vec<Cell> {
        (0..cols as u16)
            .map(|col| match screen.cell(row, col) {
                Some(cell) => {
                    let mut chars = cell.contents().chars();
                    Cell {
                        ch: chars.next().unwrap_or(' '),
                        more: chars.collect(),
                        style: Style {
                            fg: Color::from_vt(cell.fgcolor()),
                            bg: Color::from_vt(cell.bgcolor()),
                            bold: cell.bold(),
                            italic: cell.italic(),
                            underline: cell.underline(),
                            inverse: cell.inverse(),
                            dim: cell.dim(),
                        },
                        wide: cell.is_wide(),
                        continuation: cell.is_wide_continuation(),
                    }
                }
                None => Cell::default(),
            })
            .collect()
    }

    /// Every line, history first: for tests and for copying out. Drawing
    /// uses [`Self::view`], which reads only what the window shows.
    pub fn lines(&self) -> Vec<Vec<Cell>> {
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        (0..history + self.rows).map(|line| Self::line_of(&mut screen, line, history, self.cols)).collect()
    }

    /// Line `line` of history-then-screen.
    fn line_of(screen: &mut vt100::Screen, line: usize, history: usize, cols: usize) -> Vec<Cell> {
        if line < history {
            screen.set_scrollback(history - line);
            Self::row_of(screen, 0, cols)
        } else {
            screen.set_scrollback(0);
            Self::row_of(screen, (line - history) as u16, cols)
        }
    }

    /// The text from `start` to `end` — `(line, column)` in
    /// history-then-screen lines, both included — for copying a selection.
    pub fn text_between(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        let last = (history + self.rows).saturating_sub(1);
        let mut out = Vec::new();
        for line in start.0..=end.0.min(last) {
            let row = Self::line_of(&mut screen, line, history, self.cols);
            let from = if line == start.0 { start.1.min(row.len()) } else { 0 };
            let to = if line == end.0 { (end.1 + 1).min(row.len()) } else { row.len() };
            out.push(cells_text(&row[from..to.max(from)]).trim_end().to_string());
        }
        out.join("\n")
    }

    /// The screen as plain text, for tests and for copying.
    pub fn text(&self) -> String {
        self.lines().iter().map(|row| cells_text(row).trim_end().to_string()).collect::<Vec<_>>().join("\n").trim_end().to_string()
    }
}

/// The characters of a run of cells, a wide character counted once.
fn cells_text(cells: &[Cell]) -> String {
    let mut text = String::new();
    for cell in cells.iter().filter(|c| !c.continuation) {
        text.push(cell.ch);
        text.push_str(&cell.more);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds text the way a pty delivers it: the tty driver turns a
    /// program's `\n` into `\r\n` on the way out, so that is what a
    /// terminal actually receives.
    fn screen(text: &str) -> Screen {
        let mut screen = Screen::new(40, 6);
        screen.feed(text.replace('\n', "\r\n").as_bytes());
        screen
    }

    #[test]
    fn plain_output_lands_on_the_screen() {
        let screen = screen("hello\nworld\n");
        assert_eq!(screen.text(), "hello\nworld");
    }

    /// A bare line feed moves down and stays in its column — that is what
    /// the sequence means. Programs get `\r\n` because the tty driver adds
    /// the carriage return, not because the terminal invents one.
    #[test]
    fn a_bare_line_feed_does_not_return_to_column_zero() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"hello\nworld");
        assert_eq!(screen.text(), "hello\n     world");
    }

    #[test]
    fn carriage_return_overwrites_the_line() {
        // How every progress bar works.
        let screen = screen("50%\r100%");
        assert_eq!(screen.text(), "100%");
    }

    #[test]
    fn backspace_and_tab_move_the_cursor() {
        assert_eq!(screen("abc\u{8}\u{8}X").text(), "aXc");
        let screen = screen("a\tb");
        assert!(screen.text().starts_with("a       b"), "{:?}", screen.text());
    }

    #[test]
    fn colours_are_recorded_per_cell() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[31mred\x1b[0m plain");
        let line = screen.lines()[0].to_vec();
        assert_eq!(line[0].ch, 'r');
        assert_eq!(line[0].style.fg, Some(Color::Red));
        // After the reset, no colour.
        let plain = line.iter().find(|c| c.ch == 'p').unwrap();
        assert_eq!(plain.style.fg, None);
    }

    #[test]
    fn bright_and_extended_colours_parse() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[91ma\x1b[38;5;208mb\x1b[38;2;10;20;30mc");
        let line = screen.lines()[0].to_vec();
        assert_eq!(line[0].style.fg, Some(Color::Bright(1)));
        assert_eq!(line[1].style.fg, Some(Color::Indexed(208)));
        assert_eq!(line[2].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn attributes_toggle_independently() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[1;4mx\x1b[24my");
        let line = screen.lines()[0].to_vec();
        assert!(line[0].style.bold && line[0].style.underline);
        assert!(line[1].style.bold && !line[1].style.underline);
    }

    #[test]
    fn cursor_positioning_and_erase_work_together() {
        let mut screen = Screen::new(10, 3);
        screen.feed(b"aaaa\r\nbbbb\r\ncccc");
        // Home, then erase to the end of the screen.
        screen.feed(b"\x1b[1;1H\x1b[0J");
        assert_eq!(screen.text(), "");

        let mut screen = Screen::new(10, 3);
        screen.feed(b"hello");
        screen.feed(b"\x1b[3G\x1b[0K");
        assert_eq!(screen.text(), "he");
    }

    #[test]
    fn output_longer_than_the_screen_scrolls_into_history() {
        let mut screen = Screen::new(10, 2);
        screen.feed(b"one\r\ntwo\r\nthree\r\n");
        // The grid holds two rows; the rest is scrollback, and all of it is
        // still readable.
        let text = screen.text();
        assert!(text.contains("one") && text.contains("three"), "{text}");
        assert!(screen.lines().len() > 2);
    }

    #[test]
    fn a_long_line_wraps_rather_than_being_lost() {
        let mut screen = Screen::new(5, 3);
        screen.feed(b"abcdefgh");
        let text = screen.text();
        assert!(text.contains("abcde"), "{text}");
        assert!(text.contains("fgh"), "{text}");
    }

    #[test]
    fn escape_sequences_split_across_chunks_still_parse() {
        // The pty hands over whatever arrived; a colour code can be cut in
        // half by a read boundary.
        let mut screen = Screen::new(20, 3);
        screen.feed(b"\x1b[3");
        screen.feed(b"1mred");
        assert_eq!(screen.lines()[0][0].style.fg, Some(Color::Red));
        assert_eq!(screen.text(), "red");
    }

    #[test]
    fn window_titles_are_consumed_not_printed() {
        let screen = screen("\x1b]0;my title\u{7}after");
        assert_eq!(screen.text(), "after");
    }

    #[test]
    fn the_alternate_screen_is_detected_and_not_kept_in_history() {
        let mut screen = Screen::new(20, 3);
        screen.feed(b"before\r\n");
        screen.feed(b"\x1b[?1049h");
        assert!(screen.alternate_screen());
        screen.feed(b"full-screen program\r\n\r\n\r\n\r\n");
        screen.feed(b"\x1b[?1049l");
        assert!(!screen.alternate_screen());
        // What it drew is gone; what came before is not.
        assert!(!screen.text().contains("full-screen"), "{}", screen.text());
    }

    #[test]
    fn resizing_keeps_the_content() {
        let mut screen = Screen::new(20, 4);
        screen.feed(b"one\r\ntwo\r\nthree\r\n");
        screen.resize(10, 2);
        let text = screen.text();
        assert!(text.contains("one") && text.contains("three"), "{text}");
        assert_eq!(screen.cols, 10);
        assert_eq!(screen.rows, 2);
    }

    #[test]
    fn invalid_utf8_does_not_stop_the_terminal() {
        let mut screen = Screen::new(20, 3);
        screen.feed(&[0xff, 0xfe, b'o', b'k']);
        assert!(screen.text().contains("ok"));
    }

    #[test]
    fn unknown_sequences_are_dropped_rather_than_printed() {
        let screen = screen("\x1b[>4;2m\x1b[?2004hprompt");
        assert_eq!(screen.text(), "prompt");
    }

    /// What the old emulator got wrong, one by one.
    #[test]
    fn the_sequences_real_programs_send_are_handled() {
        // `tput sgr0` ends with a character-set selection; its last byte is
        // not text.
        assert_eq!(screen("\x1b[1mbold\x1b(B\x1b[m plain").text(), "bold plain");

        // A character split across two reads is one character.
        let mut split = Screen::new(20, 3);
        let bytes = "│ café ✓".as_bytes();
        split.feed(&bytes[..2]);
        split.feed(&bytes[2..9]);
        split.feed(&bytes[9..]);
        assert_eq!(split.text(), "│ café ✓");

        // A wide character takes two columns and is copied once.
        let wide = screen("日本 ok");
        let line = &wide.lines()[0];
        assert!(line[0].wide && line[1].continuation && line[4].ch == ' ', "{line:?}");
        assert_eq!(wide.text(), "日本 ok");

        // A full-screen program gets a screen of its own, and what was
        // there before it comes back when it leaves.
        let mut pager = Screen::new(20, 4);
        pager.feed(b"$ git log\r\n");
        pager.feed(b"\x1b[?1049h\x1b[H\x1b[2Jcommit abc\r\n:");
        assert!(pager.alternate_screen());
        assert_eq!(pager.view(0).rows[0][0].ch, 'c');
        pager.feed(b"\x1b[?1049l");
        assert_eq!(pager.text(), "$ git log");

        // A scroll region: the status line at the bottom stays put.
        let mut region = Screen::new(10, 4);
        region.feed(b"\x1b[4;1Hstatus\x1b[1;3r\x1b[1;1Ha\r\nb\r\nc\r\nd");
        let view = region.view(0);
        let text: Vec<String> = view.rows.iter().map(|r| cells_text(r).trim_end().to_string()).collect();
        assert_eq!(text, ["b", "c", "d", "status"]);
    }

    #[test]
    fn a_program_that_asks_the_terminal_gets_an_answer() {
        let mut screen = Screen::new(20, 5);
        assert!(screen.feed(b"plain").is_empty());
        assert_eq!(screen.feed(b"\r\nab\x1b[6n"), b"\x1b[2;3R");
        assert_eq!(screen.feed(b"\x1b[5n"), b"\x1b[0n");
        assert!(screen.feed(b"\x1b[c").starts_with(b"\x1b[?"));
        screen.feed(b"\x1b]0;my title\x07");
        assert_eq!(screen.title(), Some("my title"));
    }

    #[test]
    fn the_modes_the_keyboard_honours_are_reported() {
        let mut screen = Screen::new(20, 5);
        assert!(!screen.application_cursor() && !screen.bracketed_paste() && !screen.mouse_reporting());
        screen.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1000h");
        assert!(screen.application_cursor() && screen.bracketed_paste() && screen.mouse_reporting());
    }

    #[test]
    fn the_window_scrolls_back_through_history_and_a_selection_copies() {
        let mut screen = Screen::new(10, 2);
        screen.feed(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(screen.history(), 2);
        let live = screen.view(0);
        assert_eq!(live.cursor, Some((1, 4)));
        assert_eq!(cells_text(&live.rows[0]).trim_end(), "three");
        let back = screen.view(99);
        assert_eq!((back.offset, back.history, back.cursor), (2, 2, None));
        assert_eq!(cells_text(&back.rows[0]).trim_end(), "one");
        // Scrolling back is a view: the screen itself has not moved.
        assert_eq!(cells_text(&screen.view(0).rows[1]).trim_end(), "four");

        assert_eq!(screen.text_between((0, 1), (2, 2)), "ne\ntwo\nthr");
        assert_eq!(screen.text_between((3, 0), (3, 99)), "four");
        assert_eq!(screen.text_between((2, 2), (0, 1)), "ne\ntwo\nthr", "either direction");
    }
}
