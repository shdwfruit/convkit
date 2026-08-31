use std::collections::HashMap;
use std::path::{Path, PathBuf};

use convkit_core::{ConvError, ErrorCode, Format, Kind, Remediation};

use crate::cli::Cli;

/// One conversion to run: N inputs (almost always 1, except the image→PDF
/// merge case) producing a single output.
#[derive(Debug, Clone)]
pub struct Job {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub from: Format,
    pub to: Format,
}

/// True when the text after a leading `.` marks the bare-extension shorthand
/// (`.jpg`) rather than an ordinary relative path that merely starts with a
/// dot (`./out.gif`, `.\out.gif`, `..\out.gif`). Classifying on content, not
/// just the leading `.`, is what keeps `conv in.mp4 ./out.gif` writing
/// `out.gif` instead of misparsing the target as an unknown extension
/// `"/out.gif"`. The one case this gives up is converting to a file
/// literally named `.hidden`, where the user must write `./.hidden`.
fn is_bare_extension_shorthand(rest: &str) -> bool {
    !rest.contains('/') && !rest.contains('\\') && !rest.contains('.')
}

/// Resolves the `IN OUT` and `IN .ext` positional forms.
fn resolve_pair(paths: &[PathBuf]) -> Result<(PathBuf, PathBuf), ConvError> {
    let [input, target] = paths else {
        // I3: this is "the invocation doesn't parse," not "no recipe exists
        // for a well-formed pair" — `UnsupportedPair` means the latter, and
        // is otherwise reserved for `plan::build`/`registry::lookup`
        // failing on a pair both formats are known. A bare `conv` with no
        // arguments used to report `code: "unsupported_pair"` here, which
        // is the wrong half of spec §9's machine-readable `code` for a
        // `--json` consumer to branch on, even though the exit code (2)
        // happened to coincide either way.
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "expected an input and an output, e.g. `conv in.mp4 out.gif`",
        ));
    };
    let t = target.to_string_lossy();
    if let Some(rest) = t.strip_prefix('.') {
        if is_bare_extension_shorthand(rest) {
            if Format::from_ext(rest).is_none() {
                return Err(ConvError::unknown_format(rest));
            }
            return Ok((input.clone(), input.with_extension(rest)));
        }
    }
    Ok((input.clone(), target.clone()))
}

fn format_of(p: &Path) -> Result<Format, ConvError> {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    Format::from_ext(ext).ok_or_else(|| ConvError::unknown_format(ext))
}

/// The key two planned outputs are compared on to decide they'd land on the
/// same file. Absolute, so `./a.webp` and `a.webp` collide; the parent
/// directory is canonicalized, so `sub/../a.webp` and `a.webp` collide, and
/// so do two spellings through a symlinked directory (`std::path::absolute`
/// alone is lexical and resolves neither — both bypasses were demonstrated
/// as silent data loss). The parent, not the whole path, because the output
/// usually doesn't exist yet; when the parent doesn't either (e.g. `-o`
/// under `--dry-run`), every job falls back to the same lexical form, so
/// keys stay comparable. Case-folded on Windows and macOS, whose default
/// filesystems are case-insensitive, so `A.webp` and `a.webp` collide there
/// too — while staying distinct paths on Linux, where they genuinely are
/// distinct files. (Deliberate tradeoff: a custom case-sensitive APFS
/// volume gets a false refusal rather than Linux-on-FAT32 getting a silent
/// overwrite.) On macOS the key is additionally NFC-normalized: APFS name
/// lookup is normalization-insensitive, so an NFC `café.webp` and an NFD
/// `café.webp` are one physical file despite differing bytes.
fn collision_key(output: &Path) -> PathBuf {
    let abs = std::path::absolute(output).unwrap_or_else(|_| output.to_path_buf());
    let abs = match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent)
                .map(|real| real.join(name))
                .unwrap_or(abs)
        }
        _ => abs,
    };
    if cfg!(any(windows, target_os = "macos")) {
        let folded = abs.to_string_lossy().to_lowercase();
        #[cfg(target_os = "macos")]
        let folded: String =
            unicode_normalization::UnicodeNormalization::nfc(folded.chars()).collect();
        PathBuf::from(folded)
    } else {
        abs
    }
}

/// Natural-order comparison: a run of digits compares by numeric value, not
/// by its first character, so `p2` sorts before `p10` — plain lexicographic
/// `str` ordering does not (`'1' < '2'`, so `"p10" < "p2"`). Falls back to
/// ordinary character-by-character comparison outside of digit runs. This
/// is the ordering `plan_jobs` sorts a directory's expanded entries into
/// (I4); lexicographic would be the minimum bar, but natural order is what
/// actually matches how someone names a folder of scanned pages.
pub(crate) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut ac = a.chars().peekable();
    let mut bc = b.chars().peekable();
    loop {
        return match (ac.peek().copied(), bc.peek().copied()) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut da = String::new();
                while let Some(&c) = ac.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    da.push(c);
                    ac.next();
                }
                let mut db = String::new();
                while let Some(&c) = bc.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    db.push(c);
                    bc.next();
                }
                // Compare by numeric value (length first, since both are
                // digit-only strings with no leading-zero normalisation
                // yet — a longer run is always numerically larger once
                // leading zeros are stripped), falling back to the raw
                // digit strings only to break a tie between numerically
                // equal runs with a different count of leading zeros
                // (e.g. "07" vs "7").
                let ta = da.trim_start_matches('0');
                let tb = db.trim_start_matches('0');
                match ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)) {
                    Ordering::Equal => {
                        if da == db {
                            continue;
                        }
                        da.cmp(&db)
                    }
                    ord => ord,
                }
            }
            (Some(ca), Some(cb)) => {
                if ca == cb {
                    ac.next();
                    bc.next();
                    continue;
                }
                ca.cmp(&cb)
            }
        };
    }
}

/// Implements the batch semantics table: `--to` fans one job out per input;
/// two bare positionals are the classic pair (delegated to `resolve_pair`,
/// which also handles the `.ext` shorthand); three or more positionals whose
/// leading paths are all images and whose last is a `.pdf` become one merge
/// job. Directory expansion happens in `plan_jobs`, before this is called —
/// this function never touches the filesystem.
pub fn jobs_from(
    paths: &[PathBuf],
    to: Option<&str>,
    outdir: Option<&Path>,
) -> Result<Vec<Job>, ConvError> {
    if let Some(to_str) = to {
        let to_fmt = Format::from_ext(to_str).ok_or_else(|| ConvError::unknown_format(to_str))?;
        let mut jobs = Vec::with_capacity(paths.len());
        // Two inputs that differ only in extension (`a.jpg a.png --to webp`)
        // — or, where the filesystem is case-insensitive, only in letter
        // case — plan onto one output path. The jobs then race in rayon,
        // one result silently replaces the other, and both report OK; so
        // the collision check runs for every `--to` batch, not only under
        // `-o`, and compares `collision_key`s rather than raw paths. Maps
        // key → first input so the error can name both colliding inputs.
        let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
        for input in paths {
            let from_fmt = format_of(input)?;
            let base = input.with_extension(to_fmt.ext());
            let output = match outdir {
                Some(dir) => {
                    // I3: a path with no file name at all (e.g. `.` or `/`)
                    // is a malformed invocation, not an unsupported format
                    // pair — the formats here are perfectly well known.
                    let name = base.file_name().ok_or_else(|| {
                        ConvError::new(
                            ErrorCode::InvalidInvocation,
                            format!("input has no file name: {}", input.display()),
                        )
                    })?;
                    dir.join(name)
                }
                None => base,
            };
            if let Some(prev) = seen.insert(collision_key(&output), input.clone()) {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "outputs collide: {} and {} both produce {}",
                        prev.display(),
                        input.display(),
                        output.display()
                    ),
                ));
            }
            jobs.push(Job {
                inputs: vec![input.clone()],
                output,
                from: from_fmt,
                to: to_fmt,
            });
        }
        return Ok(jobs);
    }

    if paths.len() >= 3 {
        let (leading, last) = paths.split_at(paths.len() - 1);
        let last = &last[0];
        let last_is_pdf = Format::from_path(last) == Some(Format::Pdf);
        let all_images = leading
            .iter()
            .all(|p| Format::from_path(p).map(|f| f.kind()) == Some(Kind::Image));
        if last_is_pdf && all_images {
            let from_fmt = format_of(&leading[0])?;
            return Ok(vec![Job {
                inputs: leading.to_vec(),
                output: last.clone(),
                from: from_fmt,
                to: Format::Pdf,
            }]);
        }
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "expected images followed by a .pdf output, e.g. `conv a.png b.png out.pdf`, \
             or pass --to <format> to convert a batch of inputs",
        ));
    }

    let (input, output) = resolve_pair(paths)?;
    let from_fmt = format_of(&input)?;
    let to_fmt = format_of(&output)?;
    Ok(vec![Job {
        inputs: vec![input],
        output,
        from: from_fmt,
        to: to_fmt,
    }])
}

/// Whether a positional still carries a wildcard that nobody expanded.
///
/// `[` is deliberately not counted. `wild` treats it as a character class and
/// has already acted on it by the time this runs, so a second opinion here
/// would be too late to matter; that is a separate defect against `wild`'s
/// own expansion, not this pass.
fn has_unexpanded_wildcard(token: &str) -> bool {
    token.contains('*') || token.contains('?')
}

/// Expands the globs the shell left behind.
///
/// `wild` exists because neither cmd.exe nor PowerShell expands wildcards for
/// a native executable, but it honours the shell's quoting: a glob inside
/// quotes is passed through literally, by design. PowerShell re-quotes every
/// argument containing a space, so on a machine whose home directory is
/// `C:\Users\Rick Xie` -- a space in the path -- the README's headline batch
/// example failed with `input not found: ...\*.heic`, and `--dry-run`
/// cheerfully printed a plan with a literal `*` in both the input and the
/// output (F66). The trigger is a space anywhere in the argument, so this is
/// most of the paths a person would actually type.
///
/// Expanding here rather than switching on `wild`'s `glob-quoted-on-windows`
/// feature keeps quoting as an escape hatch: a file whose name genuinely
/// contains a wildcard character still wins, because an existing path is
/// never treated as a pattern. A pattern that matches nothing is left exactly
/// as typed, so the "input not found" error still names what the user wrote
/// rather than silently converting nothing.
///
/// `globbable` is how many leading positionals are inputs. Without `--to` the
/// last positional is the output -- in both the two-argument pair form and
/// the `a.png b.png out.pdf` merge form -- and an output must never be
/// expanded: `conv a.heic *.jpg` would otherwise turn every existing JPEG in
/// the directory into an extra input.
///
/// Matches are sorted in the same natural order directory expansion uses
/// (`p2` before `p10`), which matters because the image-to-PDF recipe merges
/// its inputs in the order given.
fn expand_globs(paths: &[PathBuf], globbable: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        let pattern = path.to_str();
        let expandable =
            i < globbable && !path.exists() && pattern.is_some_and(has_unexpanded_wildcard);
        if !expandable {
            out.push(path.clone());
            continue;
        }
        let mut matched: Vec<PathBuf> = glob::glob(pattern.expect("checked above"))
            .map(|paths| paths.flatten().collect())
            .unwrap_or_default();
        if matched.is_empty() {
            out.push(path.clone());
            continue;
        }
        matched.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        out.extend(matched);
    }
    out
}

/// Expands any directory positional into the files directly inside it
/// (non-recursive; subdirectories and files with an unrecognised extension
/// are skipped), then delegates to `jobs_from`. Globs the shell already expanded arrive
/// here as ordinary paths; the ones it quoted are expanded by
/// `expand_globs` immediately below, before any directory is read.
///
/// A file whose format already matches `--to` is skipped during this
/// expansion, regardless of whether `-o` is set: without this, `conv
/// ./photos --to jpg` (outputs beside their inputs) or `conv ./photos --to
/// jpg -o photos` (outputs into the same directory) would pick its own
/// previous output back up as fresh input on a repeat run and re-encode it,
/// degrading quality on every pass. The registry has no self-pairs, so a
/// same-format conversion could never have succeeded anyway — nothing is
/// lost by skipping it here instead of letting it fail downstream.
///
/// This only applies to files *we* chose by expanding a directory. A file
/// the user named explicitly — typed, or produced by a shell/`wild` glob
/// such as `*.jpg --to jpg` — is honoured as given and left to fail with an
/// honest unsupported-pair error; we only get to skip files we discovered
/// ourselves.
pub fn plan_jobs(cli: &Cli) -> Result<Vec<Job>, ConvError> {
    // `-o/--outdir` is a request to write there, not a precondition that it
    // already exists — `exec::run` refuses outright when its scratch
    // directory's parent is missing, and until this fix nothing ever
    // created it, so the exact invocation spec §8 and the README publish
    // (`conv ./photos --to jpg -o ./out`) failed on a fresh `./out`. A
    // failure to create it is a usage problem (the path is unwritable, or a
    // component collides with an existing file), not a conversion failure,
    // hence `InvalidInvocation` (exit 2) rather than letting it surface
    // later as an exec-time `ConversionFailed` (exit 1) once per job.
    // Skipped under `--dry-run`, which is documented as inert: a preview
    // must not leave a directory behind.
    if let Some(dir) = cli.outdir.as_ref().filter(|_| !cli.dry_run) {
        std::fs::create_dir_all(dir).map_err(|e| ConvError {
            code: ErrorCode::InvalidInvocation,
            message: format!("cannot create output directory {}: {e}", dir.display()),
            backend: None,
            remediation: Some(Remediation {
                managed: None,
                manual: Some(format!(
                    "create it yourself and check permissions, e.g. `mkdir -p {}`",
                    dir.display()
                )),
            }),
        })?;
    }

    let target_format = cli.to.as_deref().and_then(Format::from_ext);

    // Without `--to`, the last positional is the output and must not be
    // globbed. See `expand_globs`.
    let globbable = match cli.to {
        Some(_) => cli.paths.len(),
        None => cli.paths.len().saturating_sub(1),
    };
    let positionals = expand_globs(&cli.paths, globbable);

    // Without `--to`, positional grammar gives the last path an *output*
    // meaning (the pair and image-merge forms), so a directory positional
    // is only ever safe to expand as inputs when nothing it contains can
    // end up on the output side. Expanding regardless of position meant
    // `conv ./pair` (two files inside) became the pair `a.heic -> b.jpg` —
    // the folder's own second file as the output target, clobbered under
    // `-y` — and `conv in.mp4 ./somedir` did the same with somedir's first
    // file. The one no-`--to` shape where input directories are legitimate
    // is the image merge, recognisable by its explicit `.pdf` final
    // positional; everything else gets a refusal that names the fix.
    // Judged over the glob-expanded positionals, the same list `jobs_from`
    // will see.
    let merge_target = cli.to.is_none()
        && positionals.len() >= 2
        && positionals
            .last()
            .is_some_and(|p| Format::from_path(p) == Some(Format::Pdf) && !p.is_dir());

    let mut expanded: Vec<PathBuf> = Vec::with_capacity(positionals.len());
    for (idx, path) in positionals.iter().enumerate() {
        if path.is_dir() {
            if cli.to.is_none() {
                let last = idx == positionals.len() - 1;
                if last && positionals.len() >= 2 {
                    return Err(ConvError {
                        code: ErrorCode::InvalidInvocation,
                        message: format!("output target {} is a directory", path.display()),
                        backend: None,
                        remediation: Some(Remediation {
                            managed: None,
                            manual: Some(format!(
                                "write into it with `-o {}` plus `--to <format>`",
                                path.display()
                            )),
                        }),
                    });
                }
                if !merge_target {
                    return Err(ConvError {
                        code: ErrorCode::InvalidInvocation,
                        message: format!(
                            "{} is a directory; pass --to <format> to convert its contents",
                            path.display()
                        ),
                        backend: None,
                        remediation: Some(Remediation {
                            managed: None,
                            manual: Some(format!(
                                "e.g. `conv {} --to jpg`, or name the files explicitly",
                                path.display()
                            )),
                        }),
                    });
                }
            }
            // I3: an unreadable directory is a filesystem/usage problem, not
            // an unsupported format pair — no formats have even been looked
            // at yet at this point.
            let entries = std::fs::read_dir(path).map_err(|e| {
                ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!("cannot read directory {}: {e}", path.display()),
                )
            })?;
            // I4: `read_dir`'s order is arbitrary — hash order on ext4, not
            // stable even between runs on the same machine — so merging a
            // directory of scans into one PDF (the image→PDF recipe joins
            // every input, in the order given) silently shuffled pages.
            // Sort this directory's own entries into natural order (`p2`
            // before `p10`) before appending them; the relative order
            // between multiple directory/file positionals the user typed
            // is preserved, since each directory's block is sorted and
            // appended independently.
            let mut dir_entries: Vec<PathBuf> = Vec::new();
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let Some(fmt) = Format::from_path(&p) else {
                    continue;
                };
                if Some(fmt) == target_format {
                    continue;
                }
                dir_entries.push(p);
            }
            dir_entries.sort_by(|a, b| {
                natural_cmp(
                    &a.file_name().unwrap_or_default().to_string_lossy(),
                    &b.file_name().unwrap_or_default().to_string_lossy(),
                )
            });
            expanded.extend(dir_entries);
        } else {
            expanded.push(path.clone());
        }
    }
    jobs_from(&expanded, cli.to.as_deref(), cli.outdir.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn v(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    // --- quoted globs the shell left for us (F66) ----------------------------

    /// Three files whose natural order differs from their lexicographic
    /// order, in a directory whose name contains a space -- the condition
    /// that makes PowerShell quote the whole argument and `wild` leave the
    /// glob alone.
    fn photos_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("my photos");
        std::fs::create_dir(&photos).unwrap();
        for name in ["p 1.heic", "p2.heic", "p10.heic"] {
            std::fs::write(photos.join(name), b"x").unwrap();
        }
        dir
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// The README's headline Windows batch example, which failed with
    /// `input not found: ...\*.heic` on any path containing a space.
    #[test]
    fn a_glob_the_shell_quoted_is_expanded_here() {
        let dir = photos_dir();
        let pattern = dir.path().join("my photos").join("*.heic");
        let got = expand_globs(&[pattern], 1);
        assert_eq!(
            names(&got),
            vec!["p 1.heic", "p2.heic", "p10.heic"],
            "expanded, and in natural order -- image-to-PDF merges in the \
             order given, so p10 must not sort before p2"
        );
    }

    /// An output is not an input. `conv a.heic *.jpg` must convert one file,
    /// not turn every existing JPEG in the directory into an extra input.
    #[test]
    fn the_output_positional_is_never_globbed() {
        let dir = photos_dir();
        let input = dir.path().join("my photos").join("p2.heic");
        let output = dir.path().join("my photos").join("*.jpg");
        let got = expand_globs(&[input.clone(), output.clone()], 1);
        assert_eq!(got, vec![input, output]);
    }

    /// A pattern that matches nothing stays exactly as typed, so the error
    /// the user sees still names what they wrote.
    #[test]
    fn a_pattern_matching_nothing_is_left_alone() {
        let dir = photos_dir();
        let pattern = dir.path().join("my photos").join("*.tiff");
        assert_eq!(
            expand_globs(std::slice::from_ref(&pattern), 1),
            vec![pattern]
        );
    }

    #[test]
    fn a_path_with_no_wildcard_is_untouched() {
        let paths = v(&["photo.heic", "out.jpg"]);
        assert_eq!(expand_globs(&paths, 1), paths);
    }

    /// Quoting stays an escape hatch: a file whose name genuinely contains a
    /// wildcard character is used as itself, never as a pattern. Only
    /// testable off Windows, which refuses to create such a name at all.
    #[test]
    #[cfg(not(windows))]
    fn a_real_file_named_like_a_pattern_wins_over_globbing_it() {
        let dir = tempfile::tempdir().unwrap();
        let literal = dir.path().join("a*b.heic");
        std::fs::write(&literal, b"x").unwrap();
        std::fs::write(dir.path().join("axxb.heic"), b"x").unwrap();

        assert_eq!(
            expand_globs(std::slice::from_ref(&literal), 1),
            vec![literal]
        );
    }

    // --- I3: `UnsupportedPair` is reserved for a well-formed pair with no
    // recipe; these three conditions are malformed invocations instead ----

    /// A bare `conv` with no arguments used to report `code:
    /// "unsupported_pair"`, even though no pair — supported or otherwise —
    /// was ever named.
    #[test]
    fn a_missing_input_and_output_is_an_invalid_invocation_not_an_unsupported_pair() {
        let e = resolve_pair(&[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }

    #[test]
    fn bare_extension_shorthand_derives_the_output_name() {
        let (input, output) = resolve_pair(&[p("photo.heic"), p(".jpg")]).unwrap();
        assert_eq!(input, p("photo.heic"));
        assert_eq!(output, p("photo.jpg"));
    }

    #[test]
    fn bare_extension_shorthand_typo_still_suggests_a_correction() {
        let e = resolve_pair(&[p("in.mp4"), p(".gff")]).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownFormat);
        assert!(e.message.contains("did you mean"), "{}", e.message);
    }

    #[test]
    fn dot_slash_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p("./out.gif")]).unwrap();
        assert_eq!(output, p("./out.gif"));
    }

    #[test]
    fn dot_backslash_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p(r".\out.gif")]).unwrap();
        assert_eq!(output, p(r".\out.gif"));
    }

    #[test]
    fn parent_relative_path_is_not_bare_extension_shorthand() {
        let (_, output) = resolve_pair(&[p("in.mp4"), p(r"..\out.gif")]).unwrap();
        assert_eq!(output, p(r"..\out.gif"));
    }

    #[test]
    fn to_flag_makes_one_job_per_input() {
        let jobs = jobs_from(&v(&["a.heic", "b.heic"]), Some("jpg"), None).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].output, PathBuf::from("a.jpg"));
        assert_eq!(jobs[1].output, PathBuf::from("b.jpg"));
    }

    #[test]
    fn outdir_redirects_outputs() {
        let jobs = jobs_from(&v(&["x/a.heic"]), Some("jpg"), Some(Path::new("out"))).unwrap();
        assert_eq!(jobs[0].output, PathBuf::from("out").join("a.jpg"));
    }

    // --- The two data-loss paths: output collisions in any `--to` batch,
    // and directory positionals leaking into the output slot ----------------

    /// The exact bug: `conv a.jpg a.png --to webp` planned two jobs onto one
    /// `a.webp`, they raced in rayon, one result was silently lost, and both
    /// reported OK with exit 0 — because the collision check only ran under
    /// `-o`. It must run for every `--to` batch.
    #[test]
    fn same_stem_inputs_collide_even_without_outdir() {
        let e = jobs_from(&v(&["a.jpg", "a.png"]), Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
        assert!(
            e.message.contains("a.jpg") && e.message.contains("a.png"),
            "the error must name both colliding inputs: {}",
            e.message
        );
    }

    /// `./a.png` and `a.png` are one file; comparing raw paths would let
    /// them slip past the collision check.
    #[test]
    fn relative_and_absolute_spellings_of_one_output_collide() {
        let e = jobs_from(&v(&["a.jpg", "./a.png"]), Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    /// On the case-insensitive filesystems macOS and Windows default to,
    /// `A.webp` and `a.webp` are the same file, so stems differing only in
    /// case are the same silent-overwrite race with extra steps.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn outputs_differing_only_by_case_collide_on_case_insensitive_platforms() {
        let e = jobs_from(&v(&["A.jpg", "a.png"]), Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    /// Distinct stems must keep planning cleanly — the collision check may
    /// not turn into a blanket refusal of ordinary batches.
    #[test]
    fn distinct_stems_still_batch_cleanly_without_outdir() {
        let jobs = jobs_from(&v(&["a.jpg", "b.png"]), Some("webp"), None).unwrap();
        assert_eq!(jobs.len(), 2);
    }

    /// `std::path::absolute` is lexical: it resolves neither `..` nor
    /// symlinks, and both were demonstrated as silent-loss bypasses of the
    /// collision check. Canonicalizing the parent closes both.
    #[test]
    fn dot_dot_spellings_of_one_output_collide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("sub").join("..").join("a.png");
        let e = jobs_from(&[a, b], Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_spellings_of_one_output_collide() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();

        let a = real.join("x.jpg");
        let b = dir.path().join("alias").join("x.png");
        let e = jobs_from(&[a, b], Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    /// APFS name lookup is normalization-insensitive: an NFC `café.webp`
    /// and an NFD `café.webp` are one physical file despite differing
    /// bytes, so the two stems must collide on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn nfc_and_nfd_spellings_of_one_stem_collide_on_macos() {
        let e = jobs_from(
            &v(&["caf\u{e9}.jpg", "cafe\u{301}.png"]),
            Some("webp"),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    /// The exact F63 repro: `conv ./pair` where pair/ holds `a.heic` and
    /// `b.jpg` used to expand into the pair `a.heic -> b.jpg` — the user's
    /// own existing file as the output target, overwritten under `-y`. A
    /// directory alone must be refused with the `--to` remediation instead.
    #[test]
    fn a_single_directory_positional_without_to_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let pair = dir.path().join("pair");
        std::fs::create_dir(&pair).unwrap();
        std::fs::write(pair.join("a.heic"), b"input").unwrap();
        std::fs::write(pair.join("b.jpg"), b"existing photo").unwrap();

        let cli = cli_for(vec![pair], None, None);
        let e = plan_jobs(&cli).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("--to"), "{}", e.message);
        assert!(e.remediation.is_some());
    }

    /// A directory in the output position (`conv in.mp4 ./somedir`) used to
    /// be expanded too, making somedir's own first file the output target.
    /// It must be refused, pointing at `-o`.
    #[test]
    fn a_directory_in_the_output_position_is_rejected_not_expanded() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("somedir");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("precious.gif"), b"do not clobber").unwrap();
        let input = dir.path().join("in.mp4");
        std::fs::write(&input, b"x").unwrap();

        let cli = cli_for(vec![input, out.clone()], None, None);
        let e = plan_jobs(&cli).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("directory"), "{}", e.message);
        let manual = e.remediation.and_then(|r| r.manual).unwrap_or_default();
        assert!(manual.contains("-o"), "{manual}");
    }

    /// A directory paired with a non-PDF file (`conv dir b.jpg`) is neither
    /// the merge form nor a safe pair — expanding it would feed the pair
    /// grammar the same way `conv ./pair` did. Refused with the `--to` hint.
    #[test]
    fn a_directory_with_a_non_pdf_output_positional_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.heic"), b"x").unwrap();

        let cli = cli_for(vec![photos, dir.path().join("b.jpg")], None, None);
        let e = plan_jobs(&cli).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("--to"), "{}", e.message);
    }

    /// `--dry-run` is documented as inert: previewing `-o` into a directory
    /// that doesn't exist yet must not create it.
    #[test]
    fn plan_jobs_does_not_create_the_outdir_under_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.heic");
        std::fs::write(&input, b"x").unwrap();
        let outdir = dir.path().join("out");

        let mut cli = cli_for(vec![input], Some("jpg"), Some(outdir.clone()));
        cli.dry_run = true;
        let jobs = plan_jobs(&cli).unwrap();

        assert!(!outdir.exists(), "--dry-run must not create -o's target");
        assert_eq!(jobs[0].output, outdir.join("a.jpg"));
    }

    #[test]
    fn colliding_basenames_under_outdir_are_rejected() {
        let e = jobs_from(
            &v(&["x/a.heic", "y/a.heic"]),
            Some("jpg"),
            Some(Path::new("out")),
        )
        .unwrap_err();
        assert!(e.message.contains("collide"), "{}", e.message);
        assert_eq!(
            e.code,
            ErrorCode::InvalidInvocation,
            "a basename collision is a malformed invocation, not an unsupported pair"
        );
    }

    // --- I4: directory-expanded entries sort into natural order ------------

    #[test]
    fn natural_cmp_orders_numeric_runs_by_value_not_first_digit() {
        let mut v = vec!["p3", "p1", "p2", "p10"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["p1", "p2", "p3", "p10"]);
    }

    /// The exact bug: a directory of scanned pages `p3 p1 p2 p10` merged via
    /// image→PDF used to hand `magick` the inputs in `read_dir`'s arbitrary
    /// order (hash order on ext4 — not even stable between machines),
    /// silently shuffling pages. Directory expansion must sort into natural
    /// order before the merge job is built.
    #[test]
    fn plan_jobs_expands_a_directory_of_scans_in_natural_page_order() {
        let dir = tempfile::tempdir().unwrap();
        let scans = dir.path().join("scans");
        std::fs::create_dir(&scans).unwrap();
        // Written in an order that would already be wrong under both
        // filesystem-arbitrary order and plain lexicographic order
        // (`"p10" < "p2"` lexicographically).
        for name in ["p3.png", "p1.png", "p2.png", "p10.png"] {
            std::fs::write(scans.join(name), b"x").unwrap();
        }

        let cli = cli_for(vec![scans.clone(), dir.path().join("out.pdf")], None, None);
        let jobs = plan_jobs(&cli).unwrap();

        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].inputs,
            vec![
                scans.join("p1.png"),
                scans.join("p2.png"),
                scans.join("p3.png"),
                scans.join("p10.png"),
            ]
        );
    }

    #[test]
    fn a_multi_positional_invocation_that_is_not_a_valid_merge_is_an_invalid_invocation() {
        // Three positionals, last one isn't a .pdf: not the merge shape, and
        // not a 2-positional pair either.
        let e = jobs_from(&v(&["a.png", "b.png", "c.png"]), None, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }

    #[test]
    fn many_images_and_a_pdf_become_one_merge_job() {
        let jobs = jobs_from(&v(&["a.png", "b.png", "out.pdf"]), None, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs.len(), 2);
        assert_eq!(jobs[0].to, Format::Pdf);
    }

    #[test]
    fn two_positionals_are_a_single_pair_not_a_merge() {
        let jobs = jobs_from(&v(&["a.mp4", "b.gif"]), None, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs, v(&["a.mp4"]));
    }

    // --- Controller review round 4: skip already-converted files by
    // format, not by location, so re-running a batch stays idempotent
    // without disabling in-place conversion ------------------------------

    fn cli_for(paths: Vec<PathBuf>, to: Option<&str>, outdir: Option<PathBuf>) -> Cli {
        Cli {
            paths,
            to: to.map(str::to_string),
            dry_run: false,
            json: false,
            overwrite: false,
            quiet: true,
            verbose: false,
            resize: None,
            quality: None,
            colors: None,
            yes: false,
            no_install: false,
            outdir,
            jobs: None,
            ffmpeg_path: None,
            ffprobe_path: None,
            magick_path: None,
            pandoc_path: None,
            soffice_path: None,
            typst_path: None,
            command: None,
        }
    }

    /// `conv <dir> --to jpg`, no `-o`: outputs land beside their inputs, so
    /// a repeat run has exactly the same re-ingestion problem `-o` does.
    /// The guard is not tied to `-o` at all — it must skip `a.jpg` here too.
    #[test]
    fn directory_expansion_skips_files_already_in_the_target_format() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.heic"), b"fresh input").unwrap();
        std::fs::write(photos.join("a.jpg"), b"already converted").unwrap();

        let cli = cli_for(vec![photos.clone()], Some("jpg"), None);

        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].inputs, vec![photos.join("a.heic")]);
    }

    /// Updated from fix round 3: `conv ./photos --to jpg -o photos` must
    /// still convert the fresh `a.heic` — unlike the location-based guard
    /// this replaces, which excluded everything in the scanned directory
    /// whenever `-o` named that same directory. The extension-based skip
    /// only removes `a.jpg` (already the target format, and presumably a
    /// prior run's own output), leaving `a.heic` to become one job whose
    /// output lands in `photos` per `-o`.
    #[test]
    fn outdir_matching_the_scanned_directory_still_converts_fresh_input() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.heic"), b"fresh input").unwrap();
        std::fs::write(photos.join("a.jpg"), b"already converted").unwrap();

        let cli = cli_for(vec![photos.clone()], Some("jpg"), Some(photos.clone()));

        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].inputs, vec![photos.join("a.heic")]);
        assert_eq!(jobs[0].output, photos.join("a.jpg"));
    }

    // --- C2: -o/--outdir must be created, not merely assumed to exist -----

    /// The exact bug: spec §8's and the README's own headline example,
    /// `conv ./photos --to jpg -o ./out`, failed outright with "output
    /// directory does not exist" because nothing ever created `-o`'s
    /// target. `plan_jobs` must create it.
    #[test]
    fn plan_jobs_creates_the_outdir_when_it_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.heic");
        std::fs::write(&input, b"x").unwrap();
        let outdir = dir.path().join("out");
        assert!(!outdir.exists());

        let cli = cli_for(vec![input], Some("jpg"), Some(outdir.clone()));
        let jobs = plan_jobs(&cli).unwrap();

        assert!(outdir.is_dir(), "plan_jobs must create the outdir");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].output, outdir.join("a.jpg"));
    }

    /// An already-existing `-o` directory is untouched (idempotent) — a
    /// second run into the same `-o` must not error just because the
    /// directory is already there.
    #[test]
    fn plan_jobs_tolerates_an_outdir_that_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.heic");
        std::fs::write(&input, b"x").unwrap();
        let outdir = dir.path().join("out");
        std::fs::create_dir(&outdir).unwrap();

        let cli = cli_for(vec![input], Some("jpg"), Some(outdir.clone()));
        assert!(plan_jobs(&cli).is_ok());
    }

    /// When the outdir genuinely cannot be created (here: a path component
    /// collides with an existing plain file), this is a usage problem —
    /// `InvalidInvocation` (exit 2) — not a `ConversionFailed` (exit 1)
    /// surfacing once per job deep inside `exec::run`, and it must carry a
    /// remediation like every other failure (spec §9).
    #[test]
    fn plan_jobs_reports_invalid_invocation_when_the_outdir_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("blocking");
        std::fs::write(&blocking_file, b"in the way").unwrap();
        let outdir = blocking_file.join("out"); // parent is a file, not a dir

        let input = dir.path().join("a.heic");
        std::fs::write(&input, b"x").unwrap();

        let cli = cli_for(vec![input], Some("jpg"), Some(outdir));
        let e = plan_jobs(&cli).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.remediation.is_some(),
            "every failure carries a remediation"
        );
    }

    /// An explicitly named file — not discovered by expanding a directory —
    /// is honoured as given even when its format already matches `--to`:
    /// the skip only applies to files `plan_jobs` chose itself. This mirrors
    /// what a `*.jpg --to jpg` shell/`wild` glob would hand `conv`: a flat
    /// list of already-expanded paths, indistinguishable from paths the
    /// user typed by hand.
    #[test]
    fn an_explicitly_named_file_already_in_the_target_format_is_not_skipped() {
        let jobs = jobs_from(&v(&["a.jpg"]), Some("jpg"), None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs, v(&["a.jpg"]));
        assert_eq!(jobs[0].output, PathBuf::from("a.jpg"));
    }
}
