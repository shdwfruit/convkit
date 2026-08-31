use std::io::Write;
use std::path::{Path, PathBuf};

use convkit_core::{registry, ConvError, ErrorCode, Format, Kind};
use serde_json::json;

use crate::cli::Cli;
use crate::input::{expand_globs, natural_cmp};
use crate::render;

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
/// This fixes the first two and inherits the third: only regular files are
/// listed, and subdirectories are neither descended into nor shown. That is
/// a deliberate limit -- recursing by default would walk a home directory --
/// but it is a limit, so the help text states it rather than claiming
/// nothing is ever omitted.
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
        // Every positional here is an input -- there is no output slot to
        // protect -- so all of them are globbable. Without this, a quoted
        // Windows glob reached `row_for` as a literal and produced a
        // confident row for a file named `*.heic`: the same defect the
        // conversion path already fixes (see `expand_globs`), and the last
        // place it could still bite.
        expand_globs(paths, paths.len())
    };

    // Every pair the registry holds, fetched once for the whole listing
    // rather than per file. `all_pairs` builds and returns a fresh Vec, so
    // calling it per row made the work quadratic in the directory size for
    // no gain.
    let pairs = registry::all_pairs();

    let mut rows = Vec::new();
    // Collected rather than printed as they happen: under `--json` these
    // have to travel inside the envelope, and a plain line emitted alongside
    // it would leave stderr holding text-then-JSON, which parses as neither.
    let mut problems: Vec<String> = Vec::new();
    for root in &roots {
        if root.is_dir() {
            match std::fs::read_dir(root) {
                Ok(entries) => {
                    // `DirEntry::file_type` reuses what the enumeration
                    // already returned; `Path::is_file` re-stats every entry.
                    // On a large directory that is the dominant cost, and it
                    // is the one this command promises not to pay.
                    let mut here: Vec<PathBuf> = entries
                        .flatten()
                        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                        .map(|e| e.path())
                        .collect();
                    here.sort_by(|a, b| {
                        natural_cmp(
                            &a.file_name().unwrap_or_default().to_string_lossy(),
                            &b.file_name().unwrap_or_default().to_string_lossy(),
                        )
                    });
                    rows.extend(here.into_iter().map(|p| row_for(&p, &pairs)));
                }
                // A directory that cannot be read is a real failure, not an
                // empty one. Skipping it silently would make this command
                // guilty of the exact thing it exists to fix: an answer that
                // looks complete while quietly omitting things.
                Err(e) => {
                    problems.push(format!("cannot read directory {}: {e}", root.display()));
                }
            }
        } else if root.exists() {
            rows.push(row_for(root, &pairs));
        } else {
            // Naming a path that is not there is a mistake worth reporting,
            // not a file to describe. Reporting it as an ordinary row was
            // worse than useless: `conv scan missing.heic` confidently
            // printed `image -> jpg png ...` for a file that does not exist,
            // while the conversion path for the same argument says `input
            // not found`.
            problems.push(format!("not found: {}", root.display()));
        }
    }

    if cli.json {
        // The envelope must not claim success while the exit code says
        // otherwise: a run that could not list something the user named
        // reports `ok: false` in the same shape every other command uses,
        // rather than `{"ok": true}` alongside exit 2.
        if problems.is_empty() {
            print_json(&rows);
        } else {
            render::print_error(
                true,
                &ConvError::new(ErrorCode::InvalidInvocation, problems.join("; ")),
            );
        }
    } else {
        for problem in &problems {
            eprintln!("error: {problem}");
        }
        if !cli.quiet {
            print_human(&rows);
        }
    }

    // Exit 2 (InvalidInvocation) for a path the user named that could not be
    // listed -- the same code the conversion path uses for a malformed
    // invocation -- so a script can tell "nothing here" from "you asked
    // about something that isn't there". Rows that WERE found are still
    // printed: one bad argument must not discard the rest of the answer.
    if problems.is_empty() {
        0
    } else {
        ErrorCode::InvalidInvocation.exit_code()
    }
}

/// Extension in, targets out. `Format::from_path` is extension-only by
/// design, so this inherits that: a `.jpg` holding a PNG is reported as a
/// JPEG, exactly as every other convkit command would treat it.
fn row_for(path: &Path, pairs: &[(Format, Format)]) -> Row {
    let format = Format::from_path(path);
    let targets = match format {
        Some(from) => pairs
            .iter()
            .filter(|&&(f, _)| f == from)
            .map(|&(_, to)| to)
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

/// Widest the name column is allowed to get before names are ellipsized.
/// A cap that only limits *padding* does not buy a stable column -- a longer
/// name simply runs on and starts its neighbours wherever it happens to end,
/// which is worse than no alignment at all. `doctor` learned this already;
/// `truncate_name` is the same remedy.
const NAME_COLUMN: usize = 40;

fn truncate_name(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Writes the listing, tolerating a closed pipe.
///
/// `println!` panics when the write fails, and Rust does not restore the
/// default SIGPIPE handler, so `conv scan | head` died with a panic banner
/// and exit 101 -- on the single most natural thing anyone does with a long
/// listing. Writing through a locked handle and stopping quietly at the
/// first error is what `head` callers expect.
fn print_human(rows: &[Row]) {
    let names: Vec<String> = rows
        .iter()
        .map(|r| truncate_name(&display_name(&r.path), NAME_COLUMN))
        .collect();
    let name_width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (row, name) in rows.iter().zip(&names) {
        let line = match row.format {
            None => format!("{name:<name_width$}  --"),
            Some(format) => {
                let kind = kind_label(format.kind());
                if row.targets.is_empty() {
                    format!("{name:<name_width$}  {kind:<6}  no conversions")
                } else {
                    let targets: Vec<&str> = row.targets.iter().map(|t| t.ext()).collect();
                    format!("{name:<name_width$}  {kind:<6}  -> {}", targets.join(" "))
                }
            }
        };
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
}

/// The file name alone when there is one -- the listing is about what is in
/// a directory, so repeating the directory on every row is noise. Falls back
/// to the whole path for anything without a final component.
///
/// The trade is real and worth naming: with several roots the rows are
/// concatenated with no header, so two files sharing a base name render as
/// two identical lines. `--json` carries the full path for anything that
/// needs to address the file.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Serialising a `Path` directly is a panic waiting for a file name that is
/// not valid Unicode: serde's `impl Serialize for Path` returns an error for
/// one, and `json!`'s catch-all arm unwraps. A single such name in a
/// directory would have aborted the whole command with exit 101 and no
/// output. `to_string_lossy` always produces a string, so the listing
/// survives the file rather than the file killing the listing.
///
/// `kind` is `Kind`'s own `Serialize` spelling rather than the short human
/// label, so the published envelope carries the type's canonical vocabulary
/// instead of inventing a third one.
fn print_json(rows: &[Row]) {
    let files: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "path": row.path.to_string_lossy(),
                "format": row.format,
                "kind": row.format.map(|f| f.kind()),
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
        row_for(Path::new(name), &registry::all_pairs())
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
        let row = row_for(Path::new("archive.zip"), &registry::all_pairs());
        assert_eq!(row.format, None);
        assert!(row.targets.is_empty());
    }

    #[test]
    fn a_file_with_no_extension_at_all_is_reported_the_same_way() {
        let row = row_for(Path::new("README"), &registry::all_pairs());
        assert_eq!(row.format, None);
    }

    /// Extension matching is case-insensitive everywhere else in convkit,
    /// and a listing that disagreed with the converter would be worse than
    /// no listing.
    #[test]
    fn extension_matching_is_case_insensitive_like_the_rest_of_convkit() {
        assert_eq!(
            row_for(Path::new("PHOTO.HEIC"), &registry::all_pairs()).format,
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
        let row = row_for(Path::new(&format!("f.{}", orphan.ext())), &pairs);
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
            let row = row_for(Path::new(name), &pairs);
            let from = row.format.expect(name);
            for to in row.targets {
                assert!(
                    pairs.contains(&(from, to)),
                    "{name}: {from:?} -> {to:?} is not in the registry"
                );
            }
        }
    }

    /// This used to compute its expected value with the same expression as
    /// the code under test, so it could only fail if `all_pairs` were
    /// non-deterministic -- it asserted nothing about the listing. The
    /// property that actually matters is that whatever the listing offers,
    /// planning it succeeds: that is what stops the listing promising a
    /// conversion which then fails when the user asks for it.
    #[test]
    fn every_target_the_listing_offers_can_actually_be_planned() {
        let pairs = registry::all_pairs();
        for name in ["clip.mp4", "photo.heic", "notes.md", "song.flac"] {
            let row = row_for(Path::new(name), &pairs);
            let from = row.format.expect(name);
            assert!(!row.targets.is_empty(), "{name} should offer something");
            for to in &row.targets {
                let out = PathBuf::from(format!("out.{}", to.ext()));
                let planned = convkit_core::build_plan(
                    from,
                    *to,
                    std::slice::from_ref(&PathBuf::from(name)),
                    &out,
                    None,
                    None,
                );
                assert!(
                    planned.is_ok(),
                    "{name}: listing offers {from:?} -> {to:?} but planning it fails"
                );
            }
        }
    }
}
