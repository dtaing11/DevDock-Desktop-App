//! A terminal screen, and enough of the VT100/xterm sequences to drive it.
//!
//! This is not a complete emulator, and does not pretend to be. It covers
//! what command-line *output* uses — colours, cursor movement, erasing,
//! carriage returns, tabs — so that `ls`, `git`, `cargo`, and a shell
//! prompt all look right. Full-screen programs that take over the display
//! (`vim`, `top`) switch to the alternate screen; that is detected and
//! reported rather than half-rendered, because a half-drawn `vim` is worse
//! than an honest "this needs a full terminal".
//!
//! Everything here is pure: bytes in, screen out. The pty lives next door
//! in [`super`], and the parser can be tested without one.

/// One character cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', style: Style::default() }
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
        Self::from_ansi(code)
    }

    fn from_ansi(code: u8) -> Self {
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
}

/// The parser's state between bytes, since input arrives in arbitrary chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC.
    Escape,
    /// Inside a CSI sequence, collecting parameters.
    Csi { params: String, intermediate: String },
    /// Inside an OSC string, waiting for BEL or ST.
    Osc { saw_esc: bool },
}

/// Cap on scrollback, so a `cargo build` on a big workspace cannot grow
/// without limit.
const MAX_SCROLLBACK: usize = 5_000;

/// A terminal screen: a grid, a cursor, and the lines that have scrolled off.
pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    grid: Vec<Vec<Cell>>,
    scrollback: Vec<Vec<Cell>>,
    cursor: (usize, usize),
    saved_cursor: (usize, usize),
    style: Style,
    state: State,
    /// Set while a full-screen program has taken over.
    alternate: bool,
    /// Bumped on every change, so a UI can tell whether to repaint.
    pub epoch: u64,
    /// The title the program set, if any.
    pub title: Option<String>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            scrollback: Vec::new(),
            cursor: (0, 0),
            saved_cursor: (0, 0),
            style: Style::default(),
            state: State::Ground,
            alternate: false,
            epoch: 0,
            title: None,
        }
    }

    /// Whether a full-screen program is currently in control.
    pub fn alternate_screen(&self) -> bool {
        self.alternate
    }

    /// Every line, scrollback first, for rendering.
    pub fn lines(&self) -> Vec<&[Cell]> {
        self.scrollback
            .iter()
            .map(|row| row.as_slice())
            .chain(self.grid.iter().map(|row| row.as_slice()))
            .collect()
    }

    /// The cursor's position within [`Self::lines`].
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.scrollback.len() + self.cursor.0, self.cursor.1)
    }

    /// Rows and columns, keeping the content that fits.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        for row in &mut self.grid {
            row.resize(cols, Cell::default());
        }
        // Growing adds blank rows; shrinking pushes the top into scrollback,
        // which is what a real terminal does and what keeps output readable.
        while self.grid.len() > rows {
            let line = self.grid.remove(0);
            self.push_scrollback(line);
            self.cursor.0 = self.cursor.0.saturating_sub(1);
        }
        while self.grid.len() < rows {
            self.grid.push(vec![Cell::default(); cols]);
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor.0 = self.cursor.0.min(rows - 1);
        self.cursor.1 = self.cursor.1.min(cols - 1);
        self.epoch += 1;
    }

    /// Feeds output from the program.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Decode lossily: a program that emits invalid UTF-8 should not stop
        // the terminal, and the replacement character is the honest result.
        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            self.feed_char(ch);
        }
        self.epoch += 1;
    }

    fn feed_char(&mut self, ch: char) {
        match std::mem::replace(&mut self.state, State::Ground) {
            State::Ground => self.ground(ch),
            State::Escape => match ch {
                '[' => {
                    self.state = State::Csi {
                        params: String::new(),
                        intermediate: String::new(),
                    }
                }
                ']' => self.state = State::Osc { saw_esc: false },
                // Save/restore cursor, the non-CSI forms.
                '7' => self.saved_cursor = self.cursor,
                '8' => self.cursor = self.saved_cursor,
                'M' => self.reverse_index(),
                // Anything else is a sequence we do not implement; dropping
                // the escape is better than printing it as text.
                _ => {}
            },
            State::Csi { mut params, mut intermediate } => {
                match ch {
                    // Parameter bytes are 0x30-0x3F, which is `0-9:;<=>?` —
                    // the private markers `<`, `=`, `>` and `?` included.
                    // Stopping at `?` alone ends the sequence early and
                    // prints the rest of it as text.
                    '0'..='?' => {
                        params.push(ch);
                        self.state = State::Csi { params, intermediate };
                    }
                    ' '..='/' => {
                        intermediate.push(ch);
                        self.state = State::Csi { params, intermediate };
                    }
                    _ => self.csi(&params, ch),
                }
            }
            State::Osc { saw_esc } => match (saw_esc, ch) {
                // BEL or ST ends the string.
                (_, '\u{7}') => {}
                (true, '\\') => {}
                (_, '\u{1b}') => self.state = State::Osc { saw_esc: true },
                _ => self.state = State::Osc { saw_esc: false },
            },
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => self.state = State::Escape,
            '\n' => self.newline(),
            '\r' => self.cursor.1 = 0,
            '\u{8}' => self.cursor.1 = self.cursor.1.saturating_sub(1),
            '\t' => {
                let next = ((self.cursor.1 / 8) + 1) * 8;
                self.cursor.1 = next.min(self.cols - 1);
            }
            '\u{7}' => {} // bell
            ch if (ch as u32) < 0x20 => {}
            ch => self.put(ch),
        }
    }

    fn put(&mut self, ch: char) {
        if self.cursor.1 >= self.cols {
            self.newline();
            self.cursor.1 = 0;
        }
        let (row, col) = self.cursor;
        self.grid[row][col] = Cell { ch, style: self.style };
        self.cursor.1 += 1;
    }

    fn newline(&mut self) {
        if self.cursor.0 + 1 < self.rows {
            self.cursor.0 += 1;
            return;
        }
        let line = self.grid.remove(0);
        self.push_scrollback(line);
        self.grid.push(vec![Cell::default(); self.cols]);
    }

    fn reverse_index(&mut self) {
        if self.cursor.0 == 0 {
            self.grid.insert(0, vec![Cell::default(); self.cols]);
            self.grid.truncate(self.rows);
        } else {
            self.cursor.0 -= 1;
        }
    }

    fn push_scrollback(&mut self, line: Vec<Cell>) {
        // The alternate screen is transient by definition: nothing a
        // full-screen program draws belongs in the history.
        if self.alternate {
            return;
        }
        self.scrollback.push(line);
        if self.scrollback.len() > MAX_SCROLLBACK {
            let excess = self.scrollback.len() - MAX_SCROLLBACK;
            self.scrollback.drain(..excess);
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        let private = params.starts_with('?');
        let numbers: Vec<usize> = params
            .trim_start_matches(['?', '<', '=', '>'])
            .split(';')
            .map(|p| p.split(':').next().unwrap_or("").parse().unwrap_or(0))
            .collect();
        let first = numbers.first().copied().unwrap_or(0);
        let at_least_one = first.max(1);

        match final_byte {
            'A' => self.cursor.0 = self.cursor.0.saturating_sub(at_least_one),
            'B' => self.cursor.0 = (self.cursor.0 + at_least_one).min(self.rows - 1),
            'C' => self.cursor.1 = (self.cursor.1 + at_least_one).min(self.cols - 1),
            'D' => self.cursor.1 = self.cursor.1.saturating_sub(at_least_one),
            'E' => {
                self.cursor.0 = (self.cursor.0 + at_least_one).min(self.rows - 1);
                self.cursor.1 = 0;
            }
            'F' => {
                self.cursor.0 = self.cursor.0.saturating_sub(at_least_one);
                self.cursor.1 = 0;
            }
            'G' => self.cursor.1 = (at_least_one - 1).min(self.cols - 1),
            'd' => self.cursor.0 = (at_least_one - 1).min(self.rows - 1),
            'H' | 'f' => {
                let row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                let col = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cursor = (row.min(self.rows - 1), col.min(self.cols - 1));
            }
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'L' => {
                for _ in 0..at_least_one {
                    self.grid.insert(self.cursor.0, vec![Cell::default(); self.cols]);
                    self.grid.truncate(self.rows);
                }
            }
            'M' => {
                for _ in 0..at_least_one {
                    if self.cursor.0 < self.grid.len() {
                        self.grid.remove(self.cursor.0);
                        self.grid.push(vec![Cell::default(); self.cols]);
                    }
                }
            }
            'P' => {
                let row = &mut self.grid[self.cursor.0];
                for _ in 0..at_least_one {
                    if self.cursor.1 < row.len() {
                        row.remove(self.cursor.1);
                        row.push(Cell::default());
                    }
                }
            }
            'X' => {
                let (row, col) = self.cursor;
                for i in 0..at_least_one {
                    if col + i < self.cols {
                        self.grid[row][col + i] = Cell::default();
                    }
                }
            }
            'm' => self.sgr(&numbers, params),
            's' => self.saved_cursor = self.cursor,
            'u' => self.cursor = self.saved_cursor,
            // 1049/47/1047: the alternate screen. Track it so the UI can
            // say a full-screen program is running.
            'h' | 'l'
                if private && numbers.iter().any(|n| matches!(n, 1049 | 1047 | 47)) =>
            {
                self.alternate = final_byte == 'h';
                self.clear_grid();
                self.cursor = (0, 0);
            }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            // To the end of the screen.
            0 => {
                self.erase_line(0);
                for row in self.cursor.0 + 1..self.rows {
                    self.grid[row] = vec![Cell::default(); self.cols];
                }
            }
            1 => {
                self.erase_line(1);
                for row in 0..self.cursor.0 {
                    self.grid[row] = vec![Cell::default(); self.cols];
                }
            }
            // 2 clears the screen, 3 also clears scrollback.
            _ => {
                self.clear_grid();
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
        }
    }

    fn clear_grid(&mut self) {
        self.grid = vec![vec![Cell::default(); self.cols]; self.rows];
    }

    fn erase_line(&mut self, mode: usize) {
        let (row, col) = self.cursor;
        let line = &mut self.grid[row];
        match mode {
            0 => {
                for cell in line.iter_mut().skip(col) {
                    *cell = Cell::default();
                }
            }
            1 => {
                for cell in line.iter_mut().take(col + 1) {
                    *cell = Cell::default();
                }
            }
            _ => *line = vec![Cell::default(); self.cols],
        }
    }

    /// Select Graphic Rendition: colours and attributes.
    fn sgr(&mut self, numbers: &[usize], raw: &str) {
        if raw.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut i = 0;
        while i < numbers.len() {
            match numbers[i] {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.fg = Some(Color::from_ansi(numbers[i] as u8 - 30)),
                39 => self.style.fg = None,
                40..=47 => self.style.bg = Some(Color::from_ansi(numbers[i] as u8 - 40)),
                49 => self.style.bg = None,
                90..=97 => self.style.fg = Some(Color::Bright(numbers[i] as u8 - 90)),
                100..=107 => self.style.bg = Some(Color::Bright(numbers[i] as u8 - 100)),
                // 38/48 take an extended colour: 5;n for the palette, 2;r;g;b.
                38 | 48 => {
                    let foreground = numbers[i] == 38;
                    let color = match numbers.get(i + 1) {
                        Some(5) => {
                            let value = numbers.get(i + 2).copied().unwrap_or(0) as u8;
                            i += 2;
                            Some(Color::Indexed(value))
                        }
                        Some(2) => {
                            let r = numbers.get(i + 2).copied().unwrap_or(0) as u8;
                            let g = numbers.get(i + 3).copied().unwrap_or(0) as u8;
                            let b = numbers.get(i + 4).copied().unwrap_or(0) as u8;
                            i += 4;
                            Some(Color::Rgb(r, g, b))
                        }
                        _ => None,
                    };
                    if foreground {
                        self.style.fg = color;
                    } else {
                        self.style.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// The screen as plain text, for tests and for copying.
    pub fn text(&self) -> String {
        self.lines()
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }
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
}
