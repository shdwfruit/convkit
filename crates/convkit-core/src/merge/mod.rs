//! `conv merge` beyond PDFs alone: video and audio files joined one after
//! another (`clips`), and pictures placed among PDF pages (`pages`). A
//! merge of PDFs only stays with `crate::pdf`.

pub mod clips;
pub mod pages;

use std::path::Path;

/// `a`, `a and b`, `a, b and c`, with `is`/`are` to suit.
fn names_are(names: &[String]) -> (String, &'static str) {
    let verb = if names.len() == 1 { "is" } else { "are" };
    (crate::pdf::range::join_and(names), verb)
}

/// A path's file name for messages.
fn name(p: &Path) -> String {
    crate::pdf::display_name(p)
}
