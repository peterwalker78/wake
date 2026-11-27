//! Plays a terminal's recent output (`claude logs`) onto a grid the size of the session's terminal,
//! so the preview shows what that terminal shows now, not every redraw that led there.
//!
//! `claude logs` gives the raw bytes Claude wrote: cursor moves, line erases and spinner redraws.
//! Stripping the escapes from those leaves the spinner's frames run together ("Swirling…Swirling…").
//! Played onto a grid, the redraws land where they were meant to and the screen comes out as drawn.
//! Only what text needs is handled: printing, cursor moves, erases, scrolling and the alternate
//! screen. Colours and styles are dropped.

/// A terminal screen built from its output.
pub struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    /// Lines scrolled off the top, oldest first.
    scrollback: Vec<String>,
    row: usize,
    col: usize,
    /// The cursor sits past the last column; the next character wraps first.
    wrap_next: bool,
    top: usize,
    bottom: usize,
    saved: (usize, usize),
    /// The main screen, kept while a full-screen program uses the alternate one.
    main: Option<Vec<Vec<char>>>,
}

const SCROLLBACK: usize = 400;

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Grid {
        let (rows, cols) = (rows.clamp(4, 500), cols.clamp(10, 1000));
        Grid { rows, cols, cells: vec![vec![' '; cols]; rows], scrollback: vec![], row: 0, col: 0, wrap_next: false, top: 0, bottom: rows - 1, saved: (0, 0), main: None }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut p = vte::Parser::new();
        p.advance(self, bytes);
    }

    /// Scrollback, then the screen, trailing blanks trimmed.
    pub fn lines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.scrollback.clone();
        v.extend(self.cells.iter().map(|r| r.iter().collect::<String>().trim_end().to_string()));
        while v.last().is_some_and(|l| l.is_empty()) {
            v.pop();
        }
        v
    }

    fn blank(&self) -> Vec<char> {
        vec![' '; self.cols]
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let gone = self.cells.remove(self.top);
            if self.top == 0 && self.main.is_none() {
                self.scrollback.push(gone.iter().collect::<String>().trim_end().to_string());
                if self.scrollback.len() > SCROLLBACK {
                    self.scrollback.remove(0);
                }
            }
            let b = self.blank();
            self.cells.insert(self.bottom, b);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            self.cells.remove(self.bottom);
            let b = self.blank();
            self.cells.insert(self.top, b);
        }
    }

    fn line_feed(&mut self) {
        if self.row == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn goto(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.cols - 1);
        self.wrap_next = false;
    }

    fn erase(&mut self, row: usize, from: usize, to: usize) {
        let to = to.min(self.cols);
        if let Some(r) = self.cells.get_mut(row) {
            for c in r.iter_mut().take(to).skip(from) {
                *c = ' ';
            }
        }
    }
}

impl vte::Perform for Grid {
    fn print(&mut self, c: char) {
        if self.wrap_next {
            self.col = 0;
            self.line_feed();
            self.wrap_next = false;
        }
        self.cells[self.row][self.col] = c;
        if self.col + 1 < self.cols {
            self.col += 1;
        } else {
            self.wrap_next = true;
        }
    }

    fn execute(&mut self, b: u8) {
        match b {
            b'\r' => {
                self.col = 0;
                self.wrap_next = false;
            }
            b'\n' | 0x0b | 0x0c => {
                self.line_feed();
                self.wrap_next = false;
            }
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.wrap_next = false;
            }
            b'\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        let ps: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();
        let n = |i: usize| ps.get(i).copied().filter(|v| *v > 0).unwrap_or(1) as usize;
        let raw = |i: usize| ps.get(i).copied().unwrap_or(0);
        if intermediates.first() == Some(&b'?') {
            // The alternate screen: a full-screen program draws there and the main screen comes
            // back when it exits.
            if matches!(action, 'h' | 'l') && ps.iter().any(|p| matches!(p, 47 | 1047 | 1049)) {
                if action == 'h' && self.main.is_none() {
                    let b = self.blank();
                    self.main = Some(std::mem::replace(&mut self.cells, vec![b; self.rows]));
                } else if action == 'l'
                    && let Some(m) = self.main.take()
                {
                    self.cells = m;
                }
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match action {
            'A' => self.goto(self.row.saturating_sub(n(0)), self.col),
            'B' | 'e' => self.goto(self.row + n(0), self.col),
            'C' | 'a' => self.goto(self.row, self.col + n(0)),
            'D' => self.goto(self.row, self.col.saturating_sub(n(0))),
            'E' => self.goto(self.row + n(0), 0),
            'F' => self.goto(self.row.saturating_sub(n(0)), 0),
            'G' | '`' => self.goto(self.row, n(0) - 1),
            'd' => self.goto(n(0) - 1, self.col),
            'H' | 'f' => self.goto(n(0) - 1, n(1) - 1),
            'J' => {
                let (r, c) = (self.row, self.col);
                match raw(0) {
                    0 => {
                        self.erase(r, c, self.cols);
                        for i in r + 1..self.rows {
                            self.erase(i, 0, self.cols);
                        }
                    }
                    1 => {
                        for i in 0..r {
                            self.erase(i, 0, self.cols);
                        }
                        self.erase(r, 0, c + 1);
                    }
                    2 => (0..self.rows).for_each(|i| self.erase(i, 0, self.cols)),
                    3 => self.scrollback.clear(),
                    _ => {}
                }
            }
            'K' => {
                let (r, c) = (self.row, self.col);
                match raw(0) {
                    0 => self.erase(r, c, self.cols),
                    1 => self.erase(r, 0, c + 1),
                    2 => self.erase(r, 0, self.cols),
                    _ => {}
                }
            }
            'X' => {
                let (r, c) = (self.row, self.col);
                self.erase(r, c, c + n(0));
            }
            'P' => {
                let (r, c, k) = (self.row, self.col, n(0).min(self.cols - self.col));
                let line = &mut self.cells[r];
                line.drain(c..c + k);
                line.extend(std::iter::repeat_n(' ', k));
            }
            '@' => {
                let (r, c, k) = (self.row, self.col, n(0).min(self.cols - self.col));
                let line = &mut self.cells[r];
                for _ in 0..k {
                    line.insert(c, ' ');
                }
                line.truncate(self.cols);
            }
            'L' | 'M' if (self.top..=self.bottom).contains(&self.row) => {
                let top = std::mem::replace(&mut self.top, self.row);
                if action == 'L' {
                    self.scroll_down(n(0))
                } else {
                    self.scroll_up(n(0))
                }
                self.top = top;
            }
            'S' => self.scroll_up(n(0)),
            'T' => self.scroll_down(n(0)),
            'r' => {
                let top = n(0) - 1;
                let bottom = ps.get(1).copied().filter(|v| *v > 0).map_or(self.rows, |v| v as usize) - 1;
                if top < bottom && bottom < self.rows {
                    (self.top, self.bottom) = (top, bottom);
                    self.goto(0, 0);
                }
            }
            's' => self.saved = (self.row, self.col),
            'u' => self.goto(self.saved.0, self.saved.1),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.saved = (self.row, self.col),
            b'8' => self.goto(self.saved.0, self.saved.1),
            b'D' => self.line_feed(),
            b'E' => {
                self.col = 0;
                self.line_feed();
            }
            b'M' => {
                if self.row == self.top {
                    self.scroll_down(1);
                } else {
                    self.row = self.row.saturating_sub(1);
                }
            }
            b'c' => *self = Grid::new(self.rows, self.cols),
            _ => {}
        }
    }
}

/// The size of the terminal process `pid` runs in (rows, columns), read from its standard input
/// without touching it. None if it has no terminal or it can't be read.
pub fn term_size(pid: u32) -> Option<(usize, usize)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(format!("/proc/{pid}/fd/0")).ok()?;
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a winsize into `ws`, which lives for the call.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (r == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row as usize, ws.ws_col as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(rows: usize, cols: usize, s: &str) -> Vec<String> {
        let mut g = Grid::new(rows, cols);
        g.feed(s.as_bytes());
        g.lines()
    }

    #[test]
    fn spinner_redraws_land_on_one_line() {
        let s = "> hello\r\n\x1b[2;1HSwirling…\x1b[2;1H\x1b[KKneading…\x1b[2;1H\x1b[KThinking…";
        assert_eq!(play(5, 20, s), vec!["> hello", "Thinking…"]);
    }

    #[test]
    fn moves_and_erases_put_text_where_it_was_drawn() {
        let s = "\x1b[H\r\x1b[3Babc\x1b[1;5Hxyz\x1b[4;2H\x1b[X";
        assert_eq!(play(5, 10, s), vec!["    xyz", "", "", "a c"]);
    }

    #[test]
    fn long_output_scrolls_and_keeps_what_scrolled_off() {
        let s = (1..=6).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\r\n");
        assert_eq!(play(3, 10, &s), (1..=6).map(|i| format!("line {i}")).collect::<Vec<_>>());
    }

    #[test]
    fn text_wraps_at_the_edge_and_the_alternate_screen_gives_the_main_one_back() {
        assert_eq!(play(3, 10, "abcdefghijkl"), vec!["abcdefghij", "kl"]);
        assert_eq!(play(3, 10, "main\x1b[?1049hfull screen\x1b[?1049l"), vec!["main"]);
    }

    #[test]
    fn output_starting_mid_sequence_or_mid_character_is_fine() {
        let mut bytes = b"5;3Hok".to_vec();
        bytes.extend_from_slice(&"é".as_bytes()[1..]);
        bytes.extend_from_slice(b"\r\nnext");
        let mut g = Grid::new(4, 10);
        g.feed(&bytes);
        assert_eq!(g.lines().last().map(String::as_str), Some("next"));
    }

    /// `WAKE_VT_SAMPLE=<file> WAKE_VT_SIZE=rows,cols cargo test -p wake-core vt::tests::sample -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sample() {
        let b = std::fs::read(std::env::var("WAKE_VT_SAMPLE").unwrap()).unwrap();
        let size = std::env::var("WAKE_VT_SIZE").unwrap_or("40,120".into());
        let (r, c) = size.split_once(',').unwrap();
        let mut g = Grid::new(r.parse().unwrap(), c.parse().unwrap());
        g.feed(&b);
        for l in g.lines().iter().rev().take(60).rev() {
            println!("|{l}");
        }
    }

    #[test]
    fn a_terminal_size_is_only_read_from_a_terminal() {
        assert_eq!(term_size(u32::MAX), None);
    }
}
