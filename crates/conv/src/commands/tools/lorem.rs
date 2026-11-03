//! `conv lorem`: placeholder text.

use convkit_core::tools::lorem::{self as l, Amount, Markup};
use serde_json::json;

use super::Done;
use crate::cli::Cli;

pub const ABOUT: &str = "\
Prints placeholder text: a paragraph of lorem ipsum, or as many words \
(-w), sentences (-s) or paragraphs (-p) as you ask for, starting with \
\"Lorem ipsum dolor sit amet\" unless --random.

--html puts each paragraph in a <p>, and --markdown wraps the lines at 80 \
columns. --seed gives the same text every time, for tests and fixtures.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// How many words.
    #[arg(short = 'w', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100_000), conflicts_with_all = ["sentences", "paragraphs"])]
    pub words: Option<u32>,
    /// How many sentences.
    #[arg(short = 's', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=10_000), conflicts_with = "paragraphs")]
    pub sentences: Option<u32>,
    /// How many paragraphs. [default: 1]
    #[arg(short = 'p', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=1_000))]
    pub paragraphs: Option<u32>,
    /// Each paragraph in a <p>.
    #[arg(long, conflicts_with = "markdown")]
    pub html: bool,
    /// Lines wrapped at 80 columns, as in a Markdown file.
    #[arg(long)]
    pub markdown: bool,
    /// Don't start with "Lorem ipsum dolor sit amet".
    #[arg(long)]
    pub random: bool,
    /// The same text every time for the same number.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,
    /// Also copy the text to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let amount = match (args.words, args.sentences, args.paragraphs) {
        (Some(n), _, _) => Amount::Words(n as usize),
        (_, Some(n), _) => Amount::Sentences(n as usize),
        (_, _, Some(n)) => Amount::Paragraphs(n as usize),
        _ => Amount::Paragraphs(1),
    };
    let markup = if args.html {
        Markup::Html
    } else if args.markdown {
        Markup::Markdown
    } else {
        Markup::Text
    };
    let seed = args.seed.unwrap_or_else(l::random_seed);
    let paragraphs = l::generate(amount, !args.random, seed);
    let text = l::render(&paragraphs, markup);
    let json = json!({ "paragraphs": paragraphs, "text": text });
    super::finish(cli, args.copy, Done::new(text.clone(), Some(text), json))
}
