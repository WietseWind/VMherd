//! An 80x25 text screen: cells with colour and weight, a cursor, scrolling, and the few ANSI
//! sequences the demo writes itself (`ESC[...m` colours, `ESC[K` erase to end of line).

pub const COLS: usize = 80;
pub const ROWS: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Bright,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Cyan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bold: bool,
    /// Drawn with swapped colours (the cursor).
    pub inverse: bool,
}

impl Cell {
    pub const BLANK: Cell = Cell { ch: ' ', fg: Color::Default, bold: false, inverse: false };
}

pub struct Term {
    cells: Vec<Cell>,
    x: usize,
    y: usize,
    fg: Color,
    bold: bool,
    pub cursor_visible: bool,
    /// Bytes of an unfinished escape sequence (after ESC).
    esc: Option<String>,
}

impl Default for Term {
    fn default() -> Self {
        Self {
            cells: vec![Cell::BLANK; COLS * ROWS],
            x: 0,
            y: 0,
            fg: Color::Default,
            bold: false,
            cursor_visible: true,
            esc: None,
        }
    }
}

impl Term {
    pub fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.x = 0;
        self.y = 0;
    }

    pub fn write(&mut self, text: &str) {
        for c in text.chars() {
            self.put(c);
        }
    }

    fn put(&mut self, c: char) {
        if let Some(seq) = &mut self.esc {
            seq.push(c);
            if seq == "[" || !(c.is_ascii_alphabetic() || seq.len() > 16) {
                return;
            }
            let seq = self.esc.take().unwrap_or_default();
            self.escape(&seq);
            return;
        }
        match c {
            '\x1b' => self.esc = Some(String::new()),
            '\n' => self.newline(),
            '\r' => self.x = 0,
            c if c.is_control() => {}
            c => {
                if self.x >= COLS {
                    self.newline();
                }
                self.cells[self.y * COLS + self.x] = Cell { ch: c, fg: self.fg, bold: self.bold, inverse: false };
                self.x += 1;
            }
        }
    }

    /// `seq` is what followed ESC, e.g. `[1;32m`.
    fn escape(&mut self, seq: &str) {
        let Some(body) = seq.strip_prefix('[') else { return };
        let (params, cmd) = body.split_at(body.len() - 1);
        match cmd {
            "m" => {
                for p in params.split(';') {
                    match p {
                        "" | "0" => (self.fg, self.bold) = (Color::Default, false),
                        "1" => self.bold = true,
                        "2" => self.fg = Color::Dim,
                        "22" => self.bold = false,
                        "31" => self.fg = Color::Red,
                        "32" => self.fg = Color::Green,
                        "33" => self.fg = Color::Yellow,
                        "34" => self.fg = Color::Blue,
                        "36" => self.fg = Color::Cyan,
                        "37" | "97" => self.fg = Color::Bright,
                        "39" => self.fg = Color::Default,
                        _ => {}
                    }
                }
            }
            "K" => {
                let row = self.y * COLS;
                self.cells[row + self.x.min(COLS)..row + COLS].fill(Cell::BLANK);
            }
            _ => {}
        }
    }

    fn newline(&mut self) {
        self.x = 0;
        if self.y + 1 < ROWS {
            self.y += 1;
        } else {
            self.cells.copy_within(COLS.., 0);
            self.cells[(ROWS - 1) * COLS..].fill(Cell::BLANK);
        }
    }

    /// Backspace over one character (wrapping back to the previous line) and erase it.
    pub fn backspace(&mut self) {
        if self.x == 0 {
            if self.y == 0 {
                return;
            }
            self.y -= 1;
            self.x = COLS;
        }
        self.x -= 1;
        self.cells[self.y * COLS + self.x] = Cell::BLANK;
    }

    /// The cells as shown, with the cursor drawn as an inverted cell.
    pub fn snapshot(&self) -> Vec<Cell> {
        let mut cells = self.cells.clone();
        if self.cursor_visible {
            let cell = &mut cells[self.y * COLS + self.x.min(COLS - 1)];
            cell.inverse = true;
        }
        cells
    }

    /// The screen as text, trailing spaces and empty last lines removed.
    pub fn text(&self) -> String {
        let rows: Vec<String> = self
            .cells
            .chunks(COLS)
            .map(|row| row.iter().map(|c| c.ch).collect::<String>().trim_end().to_owned())
            .collect();
        rows.join("\n").trim_end().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_scrolls_and_erases() {
        let mut t = Term::default();
        t.write(&"x".repeat(COLS + 3));
        assert_eq!(t.text(), format!("{}\nxxx", "x".repeat(COLS)));
        t.backspace();
        t.backspace();
        t.backspace();
        t.backspace();
        assert_eq!((t.x, t.y), (COLS - 1, 0));
        for i in 0..ROWS + 2 {
            t.write(&format!("\nline {i}"));
        }
        let text = t.text();
        assert!(text.starts_with("line 2\n"), "{text}");
        assert!(text.ends_with(&format!("line {}", ROWS + 1)));
        t.clear();
        assert_eq!(t.text(), "");
    }

    #[test]
    fn colours_and_erase_to_end_of_line() {
        let mut t = Term::default();
        t.write("\x1b[1;32mok\x1b[0m plain\r\x1b[Kab");
        assert_eq!(t.text(), "ab");
        t.write("\n\x1b[1;32mG\x1b[0mn");
        let s = t.snapshot();
        assert_eq!(s[COLS], Cell { ch: 'G', fg: Color::Green, bold: true, inverse: false });
        assert_eq!(s[COLS + 1].fg, Color::Default);
        assert!(s[COLS + 2].inverse, "cursor after the text");
    }
}
