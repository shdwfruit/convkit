//! `conv link`: links without their tracking, out of their redirect
//! wrappers, offline unless `--follow` is asked for.

use convkit_core::tools::link::{self as l, Cleaned};
use serde_json::{json, Value};

use super::{Common, Done, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Takes the tracking out of links (utm_source and the other utm_ tags, \
fbclid, gclid, mc_eid, igshid, YouTube's and Spotify's si, and more), and \
undoes the redirect wrappers mail and chat apps put around them (Google, \
Facebook, Outlook Safe Links, Proofpoint, Slack, Steam and others), \
leaving the rest of the link exactly as it was.

conv link URL... prints each link cleaned. With -f FILE, --paste or a \
pipe, it cleans every link in the text and prints the whole text back, \
so conv link --paste --copy cleans what you copied, in place.

All of this happens offline. A short link (t.co, bit.ly and the like) \
doesn't say where it goes, so conv says so and leaves it. --follow asks \
it: one HEAD request a hop, with no cookies, downloading nothing.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Links to clean. Without them, conv cleans the links in the text
    /// from -f, --paste or a pipe.
    #[arg(value_name = "URL")]
    pub urls: Vec<String>,
    /// Ask short links where they go: this makes network requests, a HEAD
    /// request to each server along the way, with no cookies, downloading
    /// nothing.
    #[arg(long)]
    pub follow: bool,
    #[command(flatten)]
    pub io: Common,
}

/// How many redirects `--follow` goes through before it stops.
const MAX_HOPS: usize = 10;

pub fn run(cli: &Cli, args: &Args) -> i32 {
    if !args.urls.is_empty() && (args.io.file.is_some() || args.io.paste) {
        return super::fail(
            cli,
            &super::usage("give links as arguments, or a text with -f or --paste, not both"),
        );
    }
    let mut code = 0;
    let (human, links, text) = if args.urls.is_empty() {
        let text = match super::read(cli, None, &args.io, Piped::Read, "link")
            .and_then(|i| i.map(super::Input::text).transpose())
        {
            Ok(Some(t)) => t,
            Ok(None) => return super::fail(cli, &super::nothing("link", "links")),
            Err(e) => return super::fail(cli, &e),
        };
        let (cleaned, mut found) = l::clean_text(&text);
        let mut out = cleaned;
        if args.follow {
            // Links sit in `out` in order, so each is found after the last one.
            let mut at = 0;
            for c in &mut found {
                let start = at + out[at..].find(&c.output).unwrap_or(0);
                let end = start + c.output.len();
                at = end;
                // Only short links: any other link may be a one-time link a request would use up.
                if !c.short {
                    continue;
                }
                if follow(cli, c).is_err() {
                    code = 1;
                }
                out.replace_range(start..end, &c.output);
                at = start + c.output.len();
            }
        }
        if found.is_empty() {
            super::note(cli, "no http or https links in the text");
        }
        for c in &found {
            explain(cli, c, found.len() > 1, args.follow);
        }
        (out.clone(), found, Some(out))
    } else {
        let mut found = Vec::new();
        for url in &args.urls {
            match l::clean(url) {
                Ok(mut c) => {
                    if args.follow && follow(cli, &mut c).is_err() {
                        code = 1;
                    }
                    explain(cli, &c, args.urls.len() > 1, args.follow);
                    found.push(c);
                }
                Err(e) => {
                    code = 1;
                    eprint!("{}", crate::render::error_human(&super::failed(e)));
                }
            }
        }
        if found.is_empty() {
            return code;
        }
        let out: Vec<&str> = found.iter().map(|c| c.output.as_str()).collect();
        (out.join("\n"), found, None)
    };
    let json = json!({
        "links": links.iter().map(json_of).collect::<Vec<Value>>(),
        "text": text,
    });
    super::finish(
        cli,
        args.io.copy,
        Done::new(human.clone(), Some(human), json).code(code),
    )
}

/// Says what cleaning did, on stderr.
fn explain(cli: &Cli, c: &Cleaned, name_it: bool, followed: bool) {
    let mut done = Vec::new();
    for w in &c.unwrapped {
        done.push(format!("unwrapped {w}"));
    }
    if !c.removed.is_empty() {
        done.push(format!("removed {}", c.removed.join(", ")));
    }
    let prefix = if name_it {
        format!("{}: ", short(&c.input))
    } else {
        String::new()
    };
    if done.is_empty() && !c.short {
        super::note(cli, &format!("{prefix}nothing to take out"));
    } else if !done.is_empty() {
        super::note(cli, &format!("{prefix}{}", done.join("; ")));
    }
    // After --follow, a link still short is one that couldn't be
    // followed, and that was said.
    if c.short && !followed {
        let host = host_of(&c.output);
        super::note(
            cli,
            &format!(
                "{prefix}{host} is a short link, and where it goes isn't in the link; --follow asks {host} (one request, no cookies, nothing downloaded)"
            ),
        );
    }
}

/// Follows `c`'s link with the network, then cleans where it lands.
fn follow(cli: &Cli, c: &mut Cleaned) -> Result<(), ()> {
    let start = c.output.clone();
    let mut asked = Vec::new();
    let hops = l::follow(&start, MAX_HOPS, &mut |url| {
        asked.push(host_of(url));
        l::fetch_location(url)
    });
    match hops {
        Ok(hops) => {
            let last = hops.last().cloned().unwrap_or(start.clone());
            if hops.len() > 1 {
                let path: Vec<String> = hops.iter().map(|h| host_of(h)).collect();
                super::note(
                    cli,
                    &format!(
                        "followed {}: {} ({})",
                        short(&start),
                        path.join(" -> "),
                        convkit_core::tools::plural(asked.len() as u64, "request", "requests")
                    ),
                );
            } else {
                super::note(cli, &format!("{} doesn't redirect", short(&start)));
            }
            if let Ok(landed) = l::clean(&last) {
                c.removed.extend(landed.removed);
                c.unwrapped.extend(landed.unwrapped);
                c.short = landed.short;
                c.output = landed.output;
            }
            c.followed = hops;
            Ok(())
        }
        Err(e) => {
            super::warn(&format!("couldn't follow {}: {e}", short(&start)));
            Err(())
        }
    }
}

fn host_of(url: &str) -> String {
    let after = url.split_once("://").map_or(url, |(_, r)| r);
    let host = after.split(['/', '?', '#']).next().unwrap_or(after);
    let host = host.rsplit('@').next().unwrap_or(host);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

fn short(url: &str) -> String {
    if url.chars().count() > 60 {
        format!("{}...", url.chars().take(57).collect::<String>())
    } else {
        url.to_string()
    }
}

fn json_of(c: &Cleaned) -> Value {
    json!({
        "input": c.input,
        "output": c.output,
        "removed": c.removed,
        "unwrapped": c.unwrapped,
        "short": c.short,
        "followed": c.followed,
    })
}
