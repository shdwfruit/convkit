//! `conv count`: words, characters, sentences, paragraphs and lines.

use convkit_core::tools::count::{self as c, READING_WPM, SPEAKING_WPM};
use convkit_core::tools::thousands;
use serde_json::json;

use super::{Common, Done, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Counts the words, characters (with and without spaces), sentences, \
paragraphs and lines in a text, and how long it takes to read (at 238 \
words a minute) and to say (at 140).

Words and sentences follow Unicode's rules, so each Chinese or Japanese \
character counts as a word, as word processors count them. A character \
is what a reader sees as one: an emoji built from several code points is \
one. Line breaks aren't counted as characters.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The text; - reads what is piped in.
    pub text: Option<String>,
    #[command(flatten)]
    pub io: Common,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let text = match super::read(cli, args.text.as_deref(), &args.io, Piped::Read, "count")
        .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(Some(t)) => t,
        Ok(None) => return super::fail(cli, &super::nothing("count", "text")),
        Err(e) => return super::fail(cli, &e),
    };
    let n = c::count(&text);
    let rows = vec![
        ("words".to_string(), thousands(n.words)),
        ("characters".to_string(), thousands(n.characters)),
        (
            "characters, no spaces".to_string(),
            thousands(n.characters_no_spaces),
        ),
        ("sentences".to_string(), thousands(n.sentences)),
        ("paragraphs".to_string(), thousands(n.paragraphs)),
        ("lines".to_string(), thousands(n.lines)),
        ("reading time".to_string(), c::duration(n.reading_seconds)),
        ("speaking time".to_string(), c::duration(n.speaking_seconds)),
    ];
    let human = super::rows(&rows);
    let json = json!({
        "counts": [{
            "words": n.words,
            "characters": n.characters,
            "characters_no_spaces": n.characters_no_spaces,
            "sentences": n.sentences,
            "paragraphs": n.paragraphs,
            "lines": n.lines,
            "bytes": n.bytes,
            "reading_seconds": n.reading_seconds,
            "speaking_seconds": n.speaking_seconds,
            "reading_wpm": READING_WPM,
            "speaking_wpm": SPEAKING_WPM,
        }]
    });
    super::finish(
        cli,
        args.io.copy,
        Done::new(human.clone(), Some(human), json),
    )
}
