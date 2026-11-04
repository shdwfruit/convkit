//! Where the writers put their text, piece by piece and labelled, so a
//! frontend can colour it. The core itself never colours or prints.

use std::io::{self, Write};

/// What a piece of written text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Whitespace and line breaks.
    Space,
    /// Brackets, braces, commas, colons, `=`, `- `.
    Punct,
    Key,
    /// A TOML `[table]` or `[[array]]` header.
    Header,
    Str,
    Num,
    Bool,
    Null,
    Date,
}

pub trait Out {
    fn put(&mut self, role: Role, text: &str) -> io::Result<()>;
}

/// Writes the text as it is.
pub struct Plain<W: Write>(pub W);

impl<W: Write> Out for Plain<W> {
    fn put(&mut self, _: Role, text: &str) -> io::Result<()> {
        self.0.write_all(text.as_bytes())
    }
}
