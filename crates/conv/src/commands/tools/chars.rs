//! `conv chars`: every character, and the ones that hide or pretend.

use convkit_core::tools::chars::{self as c, Char, Report};
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

use super::{Common, Done, Paint, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Lists each character of a text: how it shows, its code point, category \
and Unicode name. A character built from several code points (an emoji \
with a skin tone, a flag, a letter and its accent) is listed with its \
parts under it.

conv flags what hides or pretends: invisible characters (zero-width \
spaces, joiners outside emoji, tag characters that can carry hidden text), \
bidi controls that make text read differently than it shows, lookalikes \
(a Cyrillic а in a Latin word, styled 𝐛𝐨𝐥𝐝 letters, fullwidth ones, the \
Greek question mark that looks like ;), no-break and unusual spaces, and \
control characters. The last line sums them up.

--suspicious lists only the flagged ones, with their line and column, \
for checking a file: conv chars -f config.yml --suspicious.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The text; - reads what is piped in.
    pub text: Option<String>,
    /// List only the characters conv flags, with where each one is.
    #[arg(long)]
    pub suspicious: bool,
    #[command(flatten)]
    pub io: Common,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let text = match super::read(cli, args.text.as_deref(), &args.io, Piped::Read, "chars")
        .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(Some(t)) => t,
        Ok(None) => return super::fail(cli, &super::nothing("chars", "text")),
        Err(e) => return super::fail(cli, &e),
    };
    let report = c::inspect(&text);
    let paint = Paint::stdout();
    let mut human = if args.suspicious {
        suspicious_human(&report, paint)
    } else {
        listing_human(&report, paint)
    };
    human.push_str(&summary(&report, paint));
    let json = json_of(&report, args.suspicious);
    super::finish(
        cli,
        args.io.copy,
        Done::new(human.clone(), Some(human), json),
    )
}

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pad(s: &str, to: usize) -> String {
    format!("{s}{}", " ".repeat(to.saturating_sub(width(s))))
}

fn row(ch: &Char, indent: &str, shown_width: usize, cp_width: usize, paint: Paint) -> String {
    let shown = ch.shown();
    let mut line = format!(
        "{indent}{}  {}  {}  {}",
        pad(&shown, shown_width),
        pad(&ch.code_point, cp_width),
        ch.category,
        ch.name
    );
    if let Some(f) = &ch.flag {
        line.push_str("  ");
        line.push_str(&paint.yellow(&format!("! {}", f.note)));
    }
    line.push('\n');
    line
}

fn cp_width(report: &Report) -> usize {
    report
        .chars()
        .map(|c| c.code_point.len())
        .max()
        .unwrap_or(6)
}

fn listing_human(report: &Report, paint: Paint) -> String {
    let cp = cp_width(report);
    let shown_width = report
        .chars()
        .map(|c| width(&c.shown()))
        .max()
        .unwrap_or(1)
        .clamp(1, 12);
    let mut out = String::new();
    for cluster in &report.clusters {
        if cluster.chars.len() == 1 {
            out.push_str(&row(&cluster.chars[0], "", shown_width, cp, paint));
            continue;
        }
        // The cluster as a whole, unless a part of it would break,
        // rearrange or hide the line (CR LF, a flagged character), then
        // its parts.
        let unsafe_part = cluster
            .chars
            .iter()
            .any(|c| c.flag.is_some() || matches!(c.category, "Cc" | "Zl" | "Zp"));
        let whole = if unsafe_part {
            cluster.chars.iter().map(|c| c.shown()).collect::<String>()
        } else {
            cluster.text.clone()
        };
        out.push_str(&format!(
            "{}  {}\n",
            pad(&whole, shown_width),
            paint.dim(&format!("{} code points", cluster.chars.len()))
        ));
        for ch in &cluster.chars {
            out.push_str(&row(
                ch,
                "  ",
                shown_width.saturating_sub(2).max(1),
                cp,
                paint,
            ));
        }
    }
    out
}

fn suspicious_human(report: &Report, paint: Paint) -> String {
    let flagged: Vec<&Char> = report.flagged().collect();
    let cp = flagged
        .iter()
        .map(|c| c.code_point.len())
        .max()
        .unwrap_or(6);
    let place_width = flagged
        .iter()
        .map(|c| format!("{}:{}", c.line, c.column).len())
        .max()
        .unwrap_or(0);
    let shown_width = flagged
        .iter()
        .map(|c| width(&c.shown()))
        .max()
        .unwrap_or(1)
        .clamp(1, 12);
    let mut out = String::new();
    for ch in flagged {
        let place = format!("{}:{}", ch.line, ch.column);
        out.push_str(&pad(&place, place_width));
        out.push_str("  ");
        out.push_str(&row(ch, "", shown_width, cp, paint));
    }
    out
}

fn summary(report: &Report, paint: Paint) -> String {
    let summary = report.summary();
    let total = report.chars().count();
    if summary.is_empty() {
        return format!(
            "\n{}\n",
            paint.green(&format!(
                "nothing suspicious in {}",
                convkit_core::tools::plural(total as u64, "character", "characters")
            ))
        );
    }
    let parts: Vec<String> = summary
        .iter()
        .map(|(kind, n, (line, col))| {
            let first = if *n == 1 { "at" } else { "first at" };
            format!("{} ({first} {line}:{col})", kind.count(*n))
        })
        .collect();
    format!(
        "\n{}\n",
        paint.yellow(&format!("suspicious: {}", parts.join(", ")))
    )
}

fn json_of(report: &Report, only_flagged: bool) -> Value {
    let chars: Vec<Value> = report
        .clusters
        .iter()
        .enumerate()
        .flat_map(|(i, cluster)| cluster.chars.iter().map(move |ch| (i, ch)))
        .filter(|(_, ch)| !only_flagged || ch.flag.is_some())
        .map(|(i, ch)| {
            json!({
                "char": ch.ch.to_string(),
                "code_point": ch.code_point,
                "name": ch.name,
                "category": ch.category,
                "category_name": ch.category_name,
                "script": ch.script,
                "line": ch.line,
                "column": ch.column,
                "cluster": i,
                "flag": ch.flag.as_ref().map(|f| json!({ "kind": f.kind.key(), "note": f.note })),
            })
        })
        .collect();
    let summary: Vec<Value> = report
        .summary()
        .iter()
        .map(|(kind, n, (line, col))| {
            json!({ "kind": kind.key(), "count": n, "first": { "line": line, "column": col } })
        })
        .collect();
    json!({ "chars": chars, "suspicious": summary })
}
