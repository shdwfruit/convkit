//! `conv uuid`: new UUIDs, or what one says about itself.

use convkit_core::tools::time as t;
use convkit_core::tools::uuid as u;
use serde_json::json;

use super::{Common, Done, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Makes a random UUID (version 4), or with --v7 a time-ordered one, which \
sorts in the order it was made and keeps database indexes compact. -n \
makes several.

Given a UUID, in any of the usual forms (hyphenated, 32 hex digits, \
{braced}, urn:uuid:), says what it is: its version and variant, when it \
was made for versions 1, 6 and 7, and for 1 and 6 the network address of \
the machine that made it, which such a UUID gives away.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// A UUID to read; - reads it from what is piped in. Without it, conv
    /// makes one.
    pub uuid: Option<String>,
    /// Make version 7 UUIDs: the time in milliseconds, then random bits.
    #[arg(long, conflicts_with = "uuid")]
    pub v7: bool,
    /// How many to make.
    #[arg(
        short = 'n',
        long = "count",
        value_name = "N",
        default_value_t = 1,
        value_parser = clap::value_parser!(u32).range(1..=100_000),
        conflicts_with = "uuid"
    )]
    pub count: u32,
    /// Read the UUID on the clipboard.
    #[arg(long, conflicts_with_all = ["uuid", "v7"])]
    pub paste: bool,
    /// Also copy the result to the clipboard.
    #[arg(long)]
    pub copy: bool,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let common = Common {
        file: None,
        paste: args.paste,
        copy: args.copy,
    };
    let given = match super::read(
        cli,
        args.uuid.as_deref(),
        &common,
        Piped::OnlyWithDash,
        "uuid",
    )
    .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(g) => g,
        Err(e) => return super::fail(cli, &e),
    };
    let done = match given {
        Some(text) => inspect(&text),
        None => Ok(generate(args)),
    };
    match done {
        Ok(d) => super::finish(cli, args.copy, d),
        Err(e) => super::fail(cli, &e),
    }
}

fn generate(args: &Args) -> Done {
    let ids: Vec<String> = (0..args.count)
        .map(|_| if args.v7 { u::new_v7() } else { u::new_v4() })
        .collect();
    let text = ids.join("\n");
    Done::new(text.clone(), Some(text), json!({ "uuids": ids }))
}

fn inspect(text: &str) -> Result<Done, convkit_core::ConvError> {
    let i = u::inspect(text).map_err(super::failed)?;
    let now = jiff::Timestamp::now();
    let local = jiff::tz::TimeZone::system();
    let mut rows = vec![("uuid".to_string(), i.uuid.clone())];
    rows.push((
        "version".to_string(),
        match i.version {
            Some(v) => format!("{v}, {}", i.kind),
            None => i.kind.to_string(),
        },
    ));
    rows.push(("variant".to_string(), i.variant.to_string()));
    if let Some(at) = i.timestamp {
        let shown = t::show(at, &local);
        rows.push((
            "made".to_string(),
            format!("{} ({})", shown.human, t::relative(at, now)),
        ));
    }
    if let Some(clock) = i.clock_sequence {
        rows.push(("clock".to_string(), format!("{clock:#06x}")));
    }
    if let Some(node) = &i.node {
        let what = if i.node_is_random {
            "random, not a network card's address"
        } else {
            "a network card's address: it identifies the machine that made this"
        };
        rows.push(("node".to_string(), format!("{node} ({what})")));
    }
    let json = json!({
        "uuids": [{
            "uuid": i.uuid,
            "version": i.version,
            "kind": i.kind,
            "variant": i.variant,
            "timestamp": i.timestamp.map(|t| t.to_string()),
            "clock_sequence": i.clock_sequence,
            "node": i.node,
            "node_is_random": i.node.as_ref().map(|_| i.node_is_random),
        }]
    });
    Ok(Done::new(super::rows(&rows), Some(i.uuid.clone()), json))
}
