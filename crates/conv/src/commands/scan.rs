use std::path::{Path, PathBuf};

use convkit_core::{registry, Format, Kind};
use serde_json::json;

use crate::cli::Cli;
use crate::input::natural_cmp;

/// One row of the listing: a file, and what convkit could turn it into.
struct Row {
    path: PathBuf,
    /// `None` when the extension is not one convkit knows, which is the
    /// answer to "why isn't my zip in the list" and so is worth a row rather
    /// than an omission.
    format: Option<Format>,
    targets: Vec<Format>,
}

/// Answers "what is in front of me, and what could it become" for the files
/// actually present.
///
/// `conv capabilities` already answers this globally (every pair convkit
/// knows) and per-format (`conv capabilities heic`). Neither answers it
/// contextually, and the workaround inverted the question: `conv <dir> --to
/// jpg --dry-run` makes you name a target before it will tell you anything,
/// when choosing the target is the thing you wanted help with. It also
/// reported everything that could not reach that target as an *error*, and
/// silently dropped three separate categories of file -- same-format,
/// unrecognised extension, and subdirectories -- with no way to see that it
/// had.
///
/// Deliberately a pure lookup: extension to `Format`, `Format` to the
/// registry's outgoing pairs. Nothing is opened, nothing is probed, no
/// backend is resolved. That keeps it instant on a large directory and
/// keeps its answer honest about what it actually checked -- this reports
/// what the *registry* supports, not what this machine can currently run,
/// which is `conv doctor`'s question.
pub fn run(cli: &Cli, paths: &[PathBuf]) -> i32 {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };

    let mut rows = Vec::new();
    for root in &roots {
        if root.is_dir() {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut here: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            here.sort_by(|a, b| {
                natural_cmp(
                    &a.file_name().unwrap_or_default().to_string_lossy(),
                    &b.file_name().unwrap_or_default().to_string_lossy(),
                )
            });
            rows.extend(here.into_iter().map(|p| row_for(&p)));
        } else {
            rows.push(row_for(root));
        }
    }

    if cli.json {
        print_json(&rows);
    } else {
        print_human(&rows);
    }
    0
}

/// Extension in, targets out. `Format::from_path` is extension-only by
/// design, so this inherits that: a `.jpg` holding a PNG is reported as a
/// JPEG, exactly as every other convkit command would treat it.
fn row_for(path: &Path) -> Row {
    let format = Format::from_path(path);
    let targets = match format {
        Some(from) => registry::all_pairs()
            .into_iter()
            .filter(|&(f, _)| f == from)
            .map(|(_, to)| to)
            .collect(),
        None => Vec::new(),
    };
    Row {
        path: path.to_path_buf(),
        format,
        targets,
    }
}

/// The short word shown in the kind column. Deliberately not `Kind`'s own
/// `Debug` spelling: this is user-facing text, and rendering a Rust enum's
/// name into output is how a listing starts saying `Document` where a person
/// expects `doc`.
fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Video => "video",
        Kind::Audio => "audio",
        Kind::Image => "image",
        Kind::Document => "doc",
    }
}

fn print_human(rows: &[Row]) {
    let name_width = rows
        .iter()
        .map(|r| display_name(&r.path).chars().count())
        .max()
        .unwrap_or(0)
        .min(40);

    for row in rows {
        let name = display_name(&row.path);
        match row.format {
            None => println!("{name:<name_width$}  --"),
            Some(format) => {
                let kind = kind_label(format.kind());
                if row.targets.is_empty() {
                    println!("{name:<name_width$}  {kind:<6}  no conversions");
                } else {
                    let targets: Vec<&str> = row.targets.iter().map(|t| t.ext()).collect();
                    println!("{name:<name_width$}  {kind:<6}  -> {}", targets.join(" "));
                }
            }
        }
    }
}

/// The file name alone when there is one -- the listing is about what is in
/// a directory, so repeating the directory on every row is noise. Falls back
/// to the whole path for anything without a final component.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn print_json(rows: &[Row]) {
    let files: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "path": row.path,
                "format": row.format,
                "kind": row.format.map(|f| kind_label(f.kind())),
                "convertible": !row.targets.is_empty(),
                "targets": row.targets,
            })
        })
        .collect();
    let envelope = json!({ "ok": true, "files": files });
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets_of(name: &str) -> Vec<&'static str> {
        row_for(Path::new(name))
            .targets
            .iter()
            .map(|t| t.ext())
            .collect()
    }

    /// The whole feature, stated once: a known extension yields the
    /// registry's outgoing pairs for that format.
    #[test]
    fn a_known_extension_reports_the_formats_it_can_become() {
        let targets = targets_of("photo.heic");
        assert!(targets.contains(&"jpg"), "{targets:?}");
        assert!(targets.contains(&"png"), "{targets:?}");
        assert!(!targets.contains(&"mp4"), "{targets:?}");
    }

    /// The row that makes the listing trustworthy: a file convkit has never
    /// heard of is shown, not silently dropped, so "my zip is missing" has
    /// a visible answer.
    #[test]
    fn an_unknown_extension_is_reported_rather_than_omitted() {
        let row = row_for(Path::new("archive.zip"));
        assert_eq!(row.format, None);
        assert!(row.targets.is_empty());
    }

    #[test]
    fn a_file_with_no_extension_at_all_is_reported_the_same_way() {
        let row = row_for(Path::new("README"));
        assert_eq!(row.format, None);
    }

    /// Extension matching is case-insensitive everywhere else in convkit,
    /// and a listing that disagreed with the converter would be worse than
    /// no listing.
    #[test]
    fn extension_matching_is_case_insensitive_like_the_rest_of_convkit() {
        assert_eq!(
            row_for(Path::new("PHOTO.HEIC")).format,
            Some(Format::Heic),
            "an uppercase extension must resolve exactly as the converter resolves it"
        );
    }

    /// A format convkit knows but cannot convert *out of* is a third state,
    /// distinct from both "convertible" and "never heard of it", and the
    /// human output says so rather than showing the same blank as a zip.
    /// `html` is the only such format today -- it is a target of the
    /// document recipes and the source of none -- so this derives the
    /// example from the registry instead of naming one, and stays true if
    /// that changes.
    #[test]
    fn a_known_format_with_no_outgoing_pairs_is_distinct_from_unknown() {
        let pairs = registry::all_pairs();
        let orphan = [Format::Html, Format::Pdf, Format::Docx]
            .into_iter()
            .find(|f| !pairs.iter().any(|&(from, _)| from == *f));
        let Some(orphan) = orphan else {
            return; // every format converts to something; nothing to assert
        };
        let row = row_for(Path::new(&format!("f.{}", orphan.ext())));
        assert_eq!(row.format, Some(orphan));
        assert!(row.targets.is_empty());
        assert_ne!(
            row.format, None,
            "a known-but-dead-end format must not read as an unknown extension"
        );
    }

    /// Every target reported must be a pair the registry actually holds --
    /// this is the property that stops the listing drifting from the
    /// converter as recipes are added.
    #[test]
    fn every_reported_target_is_a_real_registry_pair() {
        let pairs = registry::all_pairs();
        for name in ["photo.heic", "clip.mp4", "notes.md", "song.flac"] {
            let row = row_for(Path::new(name));
            let from = row.format.expect(name);
            for to in row.targets {
                assert!(
                    pairs.contains(&(from, to)),
                    "{name}: {from:?} -> {to:?} is not in the registry"
                );
            }
        }
    }

    /// The listing must agree with `conv capabilities <format>`, which
    /// answers the same question for a format rather than a file.
    #[test]
    fn the_listing_agrees_with_the_registrys_own_pair_list() {
        let from_registry: Vec<Format> = registry::all_pairs()
            .into_iter()
            .filter(|&(f, _)| f == Format::Mp4)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(row_for(Path::new("clip.mp4")).targets, from_registry);
    }
}
