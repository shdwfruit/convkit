//! The drawn art: a grid of cells, each a character and its colours.

/// One character cell. `fg` is the character's colour and `bg` the cell's
/// background; `None` leaves either to whatever shows the art.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
}

/// The art, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<Cell>,
}

impl Art {
    /// The characters alone: every row ends in `\n`, with its trailing
    /// spaces trimmed (paste targets trim them anyway).
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.cells.len() + self.rows as usize);
        for row in self.cells.chunks(self.cols.max(1) as usize) {
            let line: String = row.iter().map(|c| c.ch).collect();
            s.push_str(line.trim_end_matches(' '));
            s.push('\n');
        }
        s
    }

    /// Whether the art is made of half blocks, which draw a picture rather
    /// than text.
    pub fn is_blocks(&self) -> bool {
        self.cells.iter().any(|c| matches!(c.ch, '▀' | '▄'))
    }
}
