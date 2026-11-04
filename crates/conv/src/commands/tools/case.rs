//! `conv case`: change a text's case.

use convkit_core::tools::case::{self as c, Style};
use serde_json::json;

use super::{Common, Done, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Changes a text's case: upper, lower, title (Title Case, with small words \
like of and the left lowercase), sentence (Sentence case.), camel \
(camelCase), pascal (PascalCase), snake (snake_case), kebab (kebab-case) \
or constant (CONSTANT_CASE).

Letters change by Unicode's rules, so straße becomes STRASSE. Title and \
sentence case keep words like NASA and iPhone as they are. The code cases \
work a line at a time, so a list of names stays a list.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// upper, lower, title, sentence, camel, pascal, snake, kebab or
    /// constant.
    #[arg(value_name = "STYLE", value_parser = parse_style)]
    pub style: Style,
    /// The text; - reads what is piped in.
    pub text: Option<String>,
    #[command(flatten)]
    pub io: Common,
}

fn parse_style(s: &str) -> Result<Style, String> {
    Style::parse(s).ok_or_else(|| format!("use one of {}", Style::NAMES.join(", ")))
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let text = match super::read(cli, args.text.as_deref(), &args.io, Piped::Read, "case")
        .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(Some(t)) => t,
        Ok(None) => return super::fail(cli, &super::nothing("case", "text")),
        Err(e) => return super::fail(cli, &e),
    };
    let out = c::convert(&text, args.style);
    let json = json!({ "results": [{ "style": args.style.name(), "text": out }] });
    super::finish(cli, args.io.copy, Done::new(out.clone(), Some(out), json))
}
