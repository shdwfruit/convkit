use std::collections::HashSet;
use std::path::{Path, PathBuf};

use convkit_core::{ConvError, ErrorCode, Format, Kind};

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
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
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
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for input in paths {
            let from_fmt = format_of(input)?;
            let base = input.with_extension(to_fmt.ext());
            let output = match outdir {
                Some(dir) => {
                    let name = base.file_name().ok_or_else(|| {
                        ConvError::new(
                            ErrorCode::UnsupportedPair,
                            format!("input has no file name: {}", input.display()),
                        )
                    })?;
                    dir.join(name)
                }
                None => base,
            };
            if outdir.is_some() && !seen.insert(output.clone()) {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "outputs collide: more than one input produces {}",
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

/// Expands any directory positional into the files directly inside it
/// (non-recursive; subdirectories and files with an unrecognised extension
/// are skipped), then delegates to `jobs_from`. Glob expansion is already
/// done by `wild` in `main.rs` before this ever runs — this never globs.
///
/// Candidates that already sit inside `-o`'s directory are skipped: without
/// this, `conv ./photos --to jpg -o photos` would pick its own previous
/// output back up as fresh input on a repeat run and re-encode it,
/// degrading quality on every pass.
pub fn plan_jobs(cli: &Cli) -> Result<Vec<Job>, ConvError> {
    let outdir_canon = cli
        .outdir
        .as_deref()
        .and_then(|d| std::fs::canonicalize(d).ok());

    let mut expanded: Vec<PathBuf> = Vec::with_capacity(cli.paths.len());
    for path in &cli.paths {
        if path.is_dir() {
            let entries = std::fs::read_dir(path).map_err(|e| {
                ConvError::new(
                    ErrorCode::UnsupportedPair,
                    format!("cannot read directory {}: {e}", path.display()),
                )
            })?;
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                if Format::from_path(&p).is_none() {
                    continue;
                }
                if let Some(outdir) = cli.outdir.as_deref() {
                    if is_under_outdir(&p, outdir, outdir_canon.as_deref()) {
                        continue;
                    }
                }
                expanded.push(p);
            }
        } else {
            expanded.push(path.clone());
        }
    }
    jobs_from(&expanded, cli.to.as_deref(), cli.outdir.as_deref())
}

/// True when `candidate` (an existing file found while expanding a
/// directory positional) sits inside `outdir`. `outdir_canon` is `outdir`
/// canonicalized once by the caller, so this doesn't repeat that syscall
/// per candidate; canonicalizing `outdir` can fail when it doesn't exist
/// yet (`conv` creates it as needed), in which case this falls back to a
/// lexical `starts_with` rather than erroring — and a not-yet-existing
/// directory can't already contain any file `read_dir` just found anyway,
/// so the fallback is only ever exercised in practice by paranoia, not by
/// the bug this guards against.
fn is_under_outdir(candidate: &Path, outdir: &Path, outdir_canon: Option<&Path>) -> bool {
    match outdir_canon {
        Some(o) => std::fs::canonicalize(candidate)
            .map(|c| c.starts_with(o))
            .unwrap_or_else(|_| candidate.starts_with(outdir)),
        None => candidate.starts_with(outdir),
    }
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

    // --- Controller review round 3: -o must not re-ingest its own output --

    fn cli_for(paths: Vec<PathBuf>, to: Option<&str>, outdir: Option<PathBuf>) -> Cli {
        Cli {
            paths,
            to: to.map(str::to_string),
            dry_run: false,
            json: false,
            overwrite: false,
            quiet: true,
            outdir,
            jobs: None,
            ffmpeg_path: None,
            magick_path: None,
            pandoc_path: None,
            soffice_path: None,
            command: None,
        }
    }

    /// `conv ./photos --to jpg -o photos` writes `.jpg` files into `photos`
    /// itself; a repeat run must not treat `photos/a.jpg` — which IS a prior
    /// run's own output, since `-o` and the scanned directory are the same
    /// directory — as fresh input and re-encode it. Directory expansion is
    /// non-recursive, so this "candidate resolves under outdir" geometry can
    /// only arise when `-o` names the exact directory being scanned; in that
    /// geometry there is no way to tell a fresh source file (`a.heic`) apart
    /// from a previous run's output (`a.jpg`) once both live flat in the
    /// same directory, so the guard conservatively excludes every candidate
    /// there rather than guessing — the semantics table's own example
    /// already recommends a distinct `-o` directory for exactly this reason.
    #[test]
    fn scanning_a_directory_that_is_also_its_own_outdir_yields_no_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.heic"), b"fresh input").unwrap();
        std::fs::write(photos.join("a.jpg"), b"already converted").unwrap();

        let cli = cli_for(vec![photos.clone()], Some("jpg"), Some(photos.clone()));

        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(
            jobs.len(),
            0,
            "outdir == the scanned directory must exclude every candidate, \
             not just the one that looks like stale output: {jobs:?}"
        );
    }

    /// The common, recommended shape (`-o` a directory distinct from the one
    /// being scanned) must be completely unaffected by the guard above: a
    /// non-recursive scan never produces a candidate that resolves under an
    /// unrelated directory, so every fresh file is still picked up.
    #[test]
    fn a_separate_outdir_does_not_exclude_anything_being_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        let out = dir.path().join("out");
        std::fs::create_dir(&photos).unwrap();
        std::fs::create_dir(&out).unwrap();
        std::fs::write(photos.join("a.heic"), b"fresh input").unwrap();

        let cli = cli_for(vec![photos.clone()], Some("jpg"), Some(out));

        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs, vec![photos.join("a.heic")]);
    }

    /// The guard must not error when `-o` names a directory that doesn't
    /// exist yet — `conv` creates it as needed — so `canonicalize` failing
    /// on the outdir must fall back to a lexical comparison rather than
    /// propagating an error.
    #[test]
    fn directory_expansion_tolerates_an_outdir_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.heic"), b"fresh input").unwrap();

        let cli = cli_for(
            vec![photos.clone()],
            Some("jpg"),
            Some(dir.path().join("does-not-exist-yet")),
        );

        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs, vec![photos.join("a.heic")]);
    }
}
