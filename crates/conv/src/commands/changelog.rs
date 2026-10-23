//! `conv changelog [VERSION] [--since VERSION]`: what each version of conv
//! added, from the CHANGELOG.md built into it, so it works offline and
//! matches the copy that is running.

use anstyle::Style;
use convkit_core::{ConvError, ErrorCode};
use serde_json::json;

use crate::cli::Cli;
use crate::render;

/// The changelog, put in by build.rs.
pub const TEXT: &str = include_str!(concat!(env!("OUT_DIR"), "/CHANGELOG.md"));

/// Where the newest changelog is, for versions this build doesn't know.
const ONLINE: &str = "https://github.com/shdwfruit/convkit/blob/main/CHANGELOG.md";

/// One version's section: `## 0.5.1 - 2026-10-12` and the notes under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release<'a> {
    pub version: &'a str,
    pub date: &'a str,
    pub heading: &'a str,
    pub notes: &'a str,
}

/// The versions in `text`, as it lists them: newest first.
pub fn releases(text: &str) -> Vec<Release<'_>> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = find_heading(rest) {
        let section = &rest[at..];
        let line_end = section.find('\n').unwrap_or(section.len());
        let heading = section[3..line_end].trim();
        let body = &section[line_end..];
        let body_end = find_heading(body).unwrap_or(body.len());
        let (version, date) = match heading.split_once(" - ") {
            Some((v, d)) => (v.trim(), d.trim()),
            None => (heading, ""),
        };
        found.push(Release {
            version,
            date,
            heading,
            notes: body[..body_end].trim(),
        });
        rest = &body[body_end..];
    }
    found
}

/// Where the next `## ` heading starts, at the start of a line.
fn find_heading(text: &str) -> Option<usize> {
    if text.starts_with("## ") {
        return Some(0);
    }
    text.find("\n## ").map(|i| i + 1)
}

/// A version as numbers to compare, with or without a leading `v`.
fn key(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse().ok();
    let key = (next()?, next()?, next()?);
    parts.next().is_none().then_some(key)
}

/// The versions asked for: one, those after another, or all of them.
pub fn pick<'a>(
    all: &'a [Release<'a>],
    version: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<&'a Release<'a>>, ConvError> {
    let newest = all.first().map_or("", |r| r.version);
    let oldest = all.last().map_or("", |r| r.version);
    let invalid = |m: String| ConvError::new(ErrorCode::InvalidInvocation, m);
    let parse =
        |v: &str| key(v).ok_or_else(|| invalid(format!("\"{v}\" isn't a version like {newest}")));
    if let Some(v) = version {
        let wanted = parse(v)?;
        return match all.iter().find(|r| key(r.version) == Some(wanted)) {
            Some(r) => Ok(vec![r]),
            None => Err(invalid(format!(
                "this conv's changelog has no {v}; it goes from {oldest} to {newest}. The newest is at {ONLINE}"
            ))),
        };
    }
    if let Some(v) = since {
        let after = parse(v)?;
        return Ok(all
            .iter()
            .filter(|r| key(r.version) > Some(after))
            .collect());
    }
    Ok(all.iter().collect())
}

/// The versions as text. Plain, it is the changelog's own Markdown for
/// them; in a terminal, headings and `**bold**` are drawn bold and the
/// running version is marked.
pub fn human(chosen: &[&Release], installed: &str, styled: bool) -> String {
    let bold = Style::new().bold();
    let paint = |text: &str| {
        if styled {
            format!("{}{text}{}", bold.render(), bold.render_reset())
        } else {
            text.to_string()
        }
    };
    let mut out = Vec::new();
    for r in chosen {
        let mut s = String::new();
        if styled {
            s.push_str(&paint(r.heading));
            if r.version == installed {
                s.push_str("  (this version)");
            }
        } else {
            s.push_str(&format!("## {}", r.heading));
        }
        s.push_str("\n\n");
        for line in r.notes.lines() {
            if !styled {
                s.push_str(line);
            } else if let Some(title) = line.strip_prefix("### ") {
                s.push_str(&paint(title));
            } else {
                // `**` pairs around bold text, drawn bold instead.
                for (i, part) in line.split("**").enumerate() {
                    s.push_str(&if i % 2 == 1 {
                        paint(part)
                    } else {
                        part.to_string()
                    });
                }
            }
            s.push('\n');
        }
        out.push(s);
    }
    out.join("\n")
}

pub fn run(cli: &Cli, version: Option<&str>, since: Option<&str>) -> i32 {
    let all = releases(TEXT);
    let chosen = match pick(&all, version, since) {
        Ok(chosen) => chosen,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let installed = env!("CARGO_PKG_VERSION");
    if cli.json {
        let results: Vec<_> = chosen
            .iter()
            .map(|r| json!({ "version": r.version, "date": r.date, "notes": r.notes }))
            .collect();
        let v = json!({ "ok": true, "installed": installed, "results": results });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return 0;
    }
    if chosen.is_empty() {
        let after = since.unwrap_or_default();
        println!("Nothing after {after} in this conv's changelog; conv {installed} is the newest it knows.");
        return 0;
    }
    print!("{}", human(&chosen, installed, render::stdout_styled()));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog

## 0.5.1 - 2026-10-12

### New

- **Opus and Ogg.** Voice notes convert.

## 0.5.0 - 2026-10-09

### New

- **Cut a clip.** With `--start`.

## 0.4.0 - 2026-10-09

### Fixed

- A thing.
";

    #[test]
    fn releases_split_by_version() {
        let all = releases(SAMPLE);
        let versions: Vec<&str> = all.iter().map(|r| r.version).collect();
        assert_eq!(versions, ["0.5.1", "0.5.0", "0.4.0"]);
        assert_eq!(all[0].date, "2026-10-12");
        assert_eq!(
            all[0].notes,
            "### New\n\n- **Opus and Ogg.** Voice notes convert."
        );
    }

    #[test]
    fn one_version_or_those_since() {
        let all = releases(SAMPLE);
        fn names<'a>(picked: Vec<&Release<'a>>) -> Vec<&'a str> {
            picked.iter().map(|r| r.version).collect()
        }
        assert_eq!(names(pick(&all, Some("0.5.0"), None).unwrap()), ["0.5.0"]);
        assert_eq!(names(pick(&all, Some("v0.5.0"), None).unwrap()), ["0.5.0"]);
        assert_eq!(
            names(pick(&all, None, Some("0.4.0")).unwrap()),
            ["0.5.1", "0.5.0"]
        );
        // A version the changelog doesn't list still works as a start.
        assert_eq!(
            names(pick(&all, None, Some("0.4.5")).unwrap()),
            ["0.5.1", "0.5.0"]
        );
        assert!(pick(&all, None, Some("0.5.1")).unwrap().is_empty());
        assert_eq!(pick(&all, None, None).unwrap().len(), 3);
    }

    #[test]
    fn an_unknown_version_says_what_this_build_knows() {
        let all = releases(SAMPLE);
        let e = pick(&all, Some("9.9.9"), None).unwrap_err();
        assert!(e.message.contains("0.4.0 to 0.5.1"), "{}", e.message);
        assert!(e.message.contains(ONLINE), "{}", e.message);
        let e = pick(&all, None, Some("soon")).unwrap_err();
        assert!(e.message.contains("like 0.5.1"), "{}", e.message);
    }

    #[test]
    fn plain_text_is_the_changelog_itself() {
        let all = releases(SAMPLE);
        let text = human(&pick(&all, Some("0.5.1"), None).unwrap(), "0.5.1", false);
        assert_eq!(
            text,
            "## 0.5.1 - 2026-10-12\n\n### New\n\n- **Opus and Ogg.** Voice notes convert.\n"
        );
    }

    #[test]
    fn a_terminal_gets_bold_and_this_version_marked() {
        let all = releases(SAMPLE);
        let text = human(&pick(&all, None, Some("0.4.0")).unwrap(), "0.5.0", true);
        assert!(!text.contains("**") && !text.contains("## "), "{text}");
        assert!(text.contains("\x1b[1m"), "{text}");
        let marked: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("this version"))
            .collect();
        assert_eq!(marked.len(), 1, "{text}");
        assert!(marked[0].contains("0.5.0"), "{text}");
    }

    #[test]
    fn the_built_in_changelog_is_the_workspace_one() {
        assert_eq!(TEXT, include_str!("../../../../CHANGELOG.md"));
    }
}
