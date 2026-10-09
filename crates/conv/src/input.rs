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
            output_format_of_ext(rest)?;
            return Ok((input.clone(), input.with_extension(rest)));
        }
    }
    Ok((input.clone(), target.clone()))
}

fn format_of(p: &Path) -> Result<Format, ConvError> {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    Format::from_ext(ext).ok_or_else(|| ConvError::unknown_format(ext))
}

/// `format_of` for a slot that will be *written*. Same lookup, plus the one
/// refusal `format_of` must not make: an extension convkit can only read
/// (`.jfif`) names a format we know, so it parses fine as an input, but
/// handing it to a backend as an output produces a file whose bytes do not
/// match its name. Rejecting it here keeps that from ever being planned.
fn output_format_of_ext(ext: &str) -> Result<Format, ConvError> {
    let fmt = Format::from_ext(ext).ok_or_else(|| ConvError::unknown_format(ext))?;
    if Format::is_read_only_ext(ext) {
        return Err(ConvError::read_only_format(ext, fmt.ext()));
    }
    Ok(fmt)
}

fn output_format_of(p: &Path) -> Result<Format, ConvError> {
    output_format_of_ext(p.extension().and_then(|e| e.to_str()).unwrap_or(""))
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
        let to_fmt = output_format_of_ext(to_str)?;
        let mut jobs = Vec::with_capacity(paths.len());
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
            jobs.push(Job {
                inputs: vec![input.clone()],
                output,
                from: from_fmt,
                to: to_fmt,
            });
        }
        // Two inputs that differ only in extension (`a.jpg a.png --to
        // webp`) -- or, where the filesystem is case-insensitive, only in
        // letter case -- plan onto one output path. The jobs would race in
        // rayon and one result silently replace the other, so each keeps its
        // source format in its name instead. What renaming cannot keep apart
        // (one format twice, as `x/a.heic y/a.heic -o out`) is refused,
        // comparing `collision_key`s rather than raw paths.
        keep_apart(&mut jobs);
        let mut seen: HashMap<PathBuf, usize> = HashMap::new();
        for (i, job) in jobs.iter().enumerate() {
            if let Some(prev) = seen.insert(collision_key(&job.output), i) {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "outputs collide: {} and {} both produce {}",
                        jobs[prev].inputs[0].display(),
                        job.inputs[0].display(),
                        job.output.display()
                    ),
                ));
            }
        }
        return Ok(jobs);
    }

    if paths.len() >= 3 {
        if is_image_merge(paths) {
            let (leading, last) = paths.split_at(paths.len() - 1);
            let from_fmt = format_of(&leading[0])?;
            return Ok(vec![Job {
                inputs: leading.to_vec(),
                output: last[0].clone(),
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
    let to_fmt = output_format_of(&output)?;
    Ok(vec![Job {
        inputs: vec![input],
        output,
        from: from_fmt,
        to: to_fmt,
    }])
}

/// Whether three or more paths are the image-to-PDF merge form: every path
/// but the last an image, the last a `.pdf`.
fn is_image_merge(paths: &[PathBuf]) -> bool {
    let [leading @ .., last] = paths else {
        return false;
    };
    paths.len() >= 3
        && Format::from_path(last) == Some(Format::Pdf)
        && leading
            .iter()
            .all(|p| Format::from_path(p).map(|f| f.kind()) == Some(Kind::Image))
}

/// A run that can keep the input's own format: `--max-size`, which sizes a
/// video into its own container, or `--strip-metadata`, which strips most
/// image, video and audio files into their own format. A derived output
/// that would land on its input is named NAME-SUFFIX.EXT. With both flags,
/// `--max-size`'s rules and name apply: they are the narrower.
struct OwnFormat<'a> {
    sized: bool,
    /// The flag, for messages.
    flag: &'static str,
    /// What it does to each file, and what the result is called.
    verb: &'static str,
    done: &'static str,
    /// Added to a derived name: the size as typed, or `stripped`.
    suffix: &'a str,
}

impl OwnFormat<'_> {
    fn of(cli: &Cli) -> Option<OwnFormat<'_>> {
        if let Some(max) = &cli.max_size {
            return Some(OwnFormat {
                sized: true,
                flag: "--max-size",
                verb: "size",
                done: "sized",
                suffix: &max.spelling,
            });
        }
        cli.strip_metadata.then_some(OwnFormat {
            sized: false,
            flag: "--strip-metadata",
            verb: "strip",
            done: "stripped",
            suffix: "stripped",
        })
    }

    /// Whether this run can keep `f` as `f`.
    fn keeps(&self, f: Format) -> bool {
        if self.sized {
            convkit_core::sized::is_video_target(f)
        } else {
            convkit_core::metadata::in_place(f).is_ok()
        }
    }

    /// The format a batch of `f` files should name with `--to`: their own
    /// where this run keeps it, else one conv converts them to with the
    /// flag. `Err` is why there is none (the flag does not cover the
    /// format), which the caller says instead.
    fn fix_ext(&self, f: Option<Format>) -> Result<&'static str, String> {
        match f {
            Some(f) if self.keeps(f) => Ok(f.ext()),
            _ if self.sized => Ok("mp4"),
            Some(f) => convkit_core::metadata::strip_target_for(f)
                .map(|to| to.ext())
                .ok_or_else(|| {
                    convkit_core::metadata::in_place(f)
                        .err()
                        .unwrap_or_default()
                }),
            None => Ok("<format>"),
        }
    }
}

/// `conv clip.mp4 --max-size 10mb` or `conv photo.jpg --strip-metadata`:
/// the input's own format, beside the input (or in `-o`);
/// `name_own_format_outputs` adds the suffix.
fn own_format_job(
    input: &Path,
    outdir: Option<&Path>,
    own: &OwnFormat<'_>,
) -> Result<Job, ConvError> {
    let from = format_of(input)?;
    if !own.sized {
        convkit_core::metadata::in_place(from)
            .map_err(|why| ConvError::new(ErrorCode::InvalidInvocation, why))?;
    } else if !convkit_core::sized::is_video_target(from) {
        // `--to mp4` is only a fix where a conversion to mp4 exists (avi,
        // gif); for audio, other images and documents there is none, and
        // suggesting it would just move the refusal.
        let message = if convkit_core::registry::lookup(from, Format::Mp4).is_some() {
            format!(
                "--max-size keeps {}'s own format, and {} is not one it can size; add --to mp4",
                input.display(),
                from.ext()
            )
        } else {
            format!(
                "--max-size applies to video; {} is not a video file",
                input.display()
            )
        };
        return Err(ConvError::new(ErrorCode::InvalidInvocation, message));
    }
    let output = match outdir {
        Some(dir) => {
            let name = input.file_name().ok_or_else(|| {
                ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!("input has no file name: {}", input.display()),
                )
            })?;
            dir.join(name)
        }
        None => input.to_path_buf(),
    };
    Ok(Job {
        inputs: vec![input.to_path_buf()],
        output,
        from,
        to: from,
    })
}

/// A sized or stripped conversion may target its own format, so an output
/// can land on its input. A derived output gets the suffix in its name
/// (`clip-10mb.mp4`, `photo-stripped.jpg`); an explicit one is refused.
///
/// The renaming itself can make outputs collide: `clip.mp4` becomes
/// `clip-10mb.mp4`, which `clip-10mb.mov --to mp4` already plans onto, or
/// which is a previous run's output sitting in the same `*.mp4` glob. So
/// the final job set is checked once more, after renaming: no two outputs
/// may be one file, and no output may be any job's input, since that job
/// would be read while another overwrites it.
fn name_own_format_outputs(
    mut jobs: Vec<Job>,
    suffix: &str,
    explicit_output: bool,
) -> Result<Vec<Job>, ConvError> {
    for job in &mut jobs {
        let output = collision_key(&job.output);
        if job.inputs.iter().any(|i| collision_key(i) == output) {
            if explicit_output {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "output is the input: {}; name a different output",
                        job.output.display()
                    ),
                ));
            }
            job.output = suffixed_name(&job.output, suffix);
        }
    }
    // The suffix can land a sized copy on a name a conversion already
    // plans (`clip.mp4` -> `clip-80kb.mp4` beside `clip-80kb.mov`).
    keep_apart(&mut jobs);

    let input_keys: Vec<PathBuf> = jobs
        .iter()
        .flat_map(|job| job.inputs.iter().map(|i| collision_key(i)))
        .collect();
    let mut seen: HashMap<PathBuf, &Job> = HashMap::new();
    for job in &jobs {
        let key = collision_key(&job.output);
        if let Some(prev) = seen.insert(key.clone(), job) {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "outputs collide: {} and {} both produce {}",
                    prev.inputs[0].display(),
                    job.inputs[0].display(),
                    job.output.display()
                ),
            ));
        }
        if input_keys.contains(&key) {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "outputs collide: {} would write {}, which is also an input",
                    job.inputs[0].display(),
                    job.output.display()
                ),
            ));
        }
    }
    Ok(jobs)
}

/// Gives a converted file whose name another file in the same run claims,
/// as its output or as its input, its source format in its name:
/// `IMG_1.heic` beside an `IMG_1.jpg` writes `IMG_1-heic.jpg`, and `a.jpg`
/// and `a.png` to webp write `a-jpg.webp` and `a-png.webp`. Neither file is
/// lost, overwritten or refused for the other's sake. Only conversions to
/// another format are renamed: a sized or stripped copy already has its
/// own suffix, and a clash between two of the same format is left for the
/// caller to refuse, since no name can keep them apart.
fn keep_apart(jobs: &mut [Job]) {
    let source = |job: &Job| {
        job.inputs[0]
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let sources: Vec<String> = jobs.iter().map(source).collect();
    let outputs: Vec<PathBuf> = jobs.iter().map(|j| collision_key(&j.output)).collect();
    let inputs: Vec<(usize, PathBuf)> = jobs
        .iter()
        .enumerate()
        .flat_map(|(i, j)| j.inputs.iter().map(move |p| (i, collision_key(p))))
        .collect();
    // Claimed by something a different name can get away from: another
    // job's input, or another job's output from a different format.
    let claimed: Vec<bool> = outputs
        .iter()
        .enumerate()
        .map(|(i, key)| {
            outputs
                .iter()
                .enumerate()
                .any(|(k, o)| k != i && o == key && sources[k] != sources[i])
                || inputs.iter().any(|(k, p)| *k != i && p == key)
        })
        .collect();
    for ((job, claimed), source) in jobs.iter_mut().zip(claimed).zip(sources) {
        if claimed && job.from != job.to && job.inputs.len() == 1 {
            job.output = suffixed_name(&job.output, &source);
        }
    }
}

/// `clip.mp4` + `10mb` -> `clip-10mb.mp4`, keeping the extension's case.
fn suffixed_name(path: &Path, spelling: &str) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    match path.extension() {
        Some(ext) => path.with_file_name(format!("{stem}-{spelling}.{}", ext.to_string_lossy())),
        None => path.with_file_name(format!("{stem}-{spelling}")),
    }
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
pub(crate) fn expand_globs(paths: &[PathBuf], globbable: usize) -> Vec<PathBuf> {
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
        create_output_dir(dir)?;
    }

    let target_format = cli.to.as_deref().and_then(Format::from_ext);

    // Without `--to`, the last positional is the output and must not be
    // globbed. See `expand_globs`.
    // The exception is a lone path under `--max-size` or
    // `--strip-metadata`: that form has no output positional at all, so the
    // one path is an input.
    let own = OwnFormat::of(cli);
    let lone_sized_input = own.is_some() && cli.to.is_none() && cli.paths.len() == 1;
    let globbable = match cli.to {
        Some(_) => cli.paths.len(),
        None if lone_sized_input => 1,
        None => cli.paths.len().saturating_sub(1),
    };
    let positionals = expand_globs(&cli.paths, globbable);
    // A lone pattern that matched several files must not slide into the
    // `IN OUT` pair form, where the second match would be an output.
    if let (true, Some(own)) = (lone_sized_input && positionals.len() > 1, &own) {
        let pattern = &cli.paths[0];
        let message = match own.fix_ext(Format::from_path(pattern)) {
            Ok(ext) => format!(
                "{} matched {} files; add --to {ext} to {} each one",
                pattern.display(),
                positionals.len(),
                own.verb
            ),
            Err(why) => format!(
                "{} matched {} files, and {why}",
                pattern.display(),
                positionals.len()
            ),
        };
        return Err(ConvError::new(ErrorCode::InvalidInvocation, message));
    }

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
                    // Under `--max-size` or `--strip-metadata` a file already
                    // in the target format is what gets sized or stripped, so
                    // it is kept, where the flag can keep that format; a
                    // previous run's own result (`clip-10mb.mp4`,
                    // `photo-stripped.jpg`) is left behind, and so is a file
                    // the flag cannot keep (a pdf among scans), as it is
                    // without the flag.
                    let kept_here = own.as_ref().is_some_and(|own| {
                        own.keeps(fmt)
                            && !p.file_stem().is_some_and(|stem| {
                                stem.to_string_lossy()
                                    .to_lowercase()
                                    .ends_with(&format!("-{}", own.suffix))
                            })
                    });
                    if !kept_here {
                        continue;
                    }
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
    let Some(own) = own else {
        return jobs_from(&expanded, cli.to.as_deref(), cli.outdir.as_deref());
    };
    // A typed-out OUT is the user's explicit choice and is never renamed;
    // the `.ext` shorthand and every --to output are derived.
    let explicit_output = cli.to.is_none()
        && expanded.len() == 2
        && !expanded[1]
            .to_string_lossy()
            .strip_prefix('.')
            .is_some_and(is_bare_extension_shorthand);
    if cli.to.is_none() {
        refuse_a_batch_without_to(&expanded, explicit_output, &own)?;
    }
    // The one form these flags add: a lone path keeps its own format.
    let mut jobs = match (cli.to.as_deref(), expanded.as_slice()) {
        (None, [single]) => vec![own_format_job(single, cli.outdir.as_deref(), &own)?],
        _ => jobs_from(&expanded, cli.to.as_deref(), cli.outdir.as_deref())?,
    };
    // A batch writes into a subfolder beside its inputs (`photos/stripped/`,
    // `clips/10mb/`) unless `-o` says where. These runs take files already
    // in the target format as inputs, so outputs written among them would
    // be taken for new inputs by the next run, which would size or strip
    // its own results again. A folder input never reads its subfolders, so
    // a re-run converts only what is new; what is already in the subfolder
    // is an existing file, never overwritten without -y.
    let into_subfolder = cli.to.is_some() && cli.outdir.is_none();
    if into_subfolder {
        for job in &mut jobs {
            job.output = in_subfolder(&job.output, own.suffix);
        }
    }
    let jobs = name_own_format_outputs(jobs, own.suffix, explicit_output)?;
    if into_subfolder && !cli.dry_run {
        let mut made: Vec<&Path> = Vec::new();
        for dir in jobs.iter().filter_map(|j| j.output.parent()) {
            if !made.contains(&dir) {
                create_output_dir(dir)?;
                made.push(dir);
            }
        }
    }
    Ok(jobs)
}

/// `photos/IMG_2.jpg` + `stripped` -> `photos/stripped/IMG_2.jpg`.
fn in_subfolder(output: &Path, folder: &str) -> PathBuf {
    let name = output.file_name().unwrap_or_default();
    match output.parent() {
        Some(dir) => dir.join(folder).join(name),
        None => Path::new(folder).join(name),
    }
}

/// Creates an output directory, or says why it cannot be: a usage problem
/// (an unwritable path, or a file in the way), not a conversion failure.
fn create_output_dir(dir: &Path) -> Result<(), ConvError> {
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
    })
}

/// Under `--max-size` or `--strip-metadata` with no `--to`, a glob
/// (`conv *.mp4 --max-size 8mb`, `conv *.jpg --strip-metadata`) reaches conv
/// as a list of paths that the positional grammar reads as something else.
/// Two paths are the `IN OUT` pair, so sizing them would replace the second
/// clip with a sized copy of the first; three or more are the image-to-PDF
/// merge form. Both are refused here with `--to` as the fix. A pair whose
/// output already exists in the input's own format, one the flag can keep,
/// is refused even with `-y`, since that is exactly what the glob produces;
/// to write a sized or stripped copy over a file of the same format, remove
/// it first. An output that is the input itself is left to
/// `name_own_format_outputs`, which words that case.
fn refuse_a_batch_without_to(
    paths: &[PathBuf],
    explicit_output: bool,
    own: &OwnFormat<'_>,
) -> Result<(), ConvError> {
    match paths {
        [input, output] if explicit_output && output.exists() => {
            // Only a format the flag can keep gets `--to` as the fix; for any
            // other pair it would just move the refusal, so the pair is left
            // to the refusal of the flag on that target.
            let from = Format::from_path(input).filter(|f| own.keeps(*f));
            if from.is_none()
                || from != Format::from_path(output)
                || collision_key(input) == collision_key(output)
            {
                return Ok(());
            }
            let ext = from.expect("checked above").ext();
            Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{} and {} are both {ext} files; add --to {ext} to {} each one, \
                     or remove {} to write a {} copy there",
                    input.display(),
                    output.display(),
                    own.verb,
                    output.display(),
                    own.done
                ),
            ))
        }
        [first, _, _, ..] if !is_image_merge(paths) => {
            let message = match own.fix_ext(Format::from_path(first)) {
                Ok(ext) => format!(
                    "{} without --to takes one input, or an input and an output; \
                     add --to {ext} to {} each file",
                    own.flag, own.verb
                ),
                Err(why) => why,
            };
            Err(ConvError::new(ErrorCode::InvalidInvocation, message))
        }
        _ => Ok(()),
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

    fn outputs(jobs: &[Job]) -> Vec<PathBuf> {
        jobs.iter().map(|j| j.output.clone()).collect()
    }

    /// The outputs as paths under `dir`, `/`-separated on every platform.
    fn under(dir: &Path, jobs: &[Job]) -> Vec<String> {
        jobs.iter()
            .map(|j| {
                let rel = j.output.strip_prefix(dir).unwrap_or(&j.output);
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
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

    // --- read-only extensions: readable as input, refused as output -----

    /// `.jfif` is a JPEG by another name, so it plans like one on the way in.
    #[test]
    fn a_read_only_extension_is_accepted_as_an_input() {
        let jobs = jobs_from(&v(&["photo.jfif"]), Some("png"), None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].from, Format::Jpg);
        assert_eq!(jobs[0].to, Format::Png);
    }

    /// Every slot that names an output refuses it, because ImageMagick has
    /// no JFIF coder and would write the input's bytes under that name. The
    /// error names the spelling convkit does write.
    #[test]
    fn a_read_only_extension_is_refused_in_every_output_slot() {
        // the explicit pair
        let e = jobs_from(&v(&["a.png", "b.jfif"]), None, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("jpg"), "{}", e.message);

        // the `.ext` shorthand
        let e = jobs_from(&v(&["a.png", ".jfif"]), None, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);

        // and --to
        let e = jobs_from(&v(&["a.png"]), Some("jfif"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.remediation.is_some(), "every refusal carries a fix");
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
        // The same file named twice: one format, so no name keeps them apart.
        let e = jobs_from(&v(&["a.jpg", "a.jpg"]), Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "outputs collide: a.jpg and a.jpg both produce a.webp"
        );
    }

    /// Two inputs that differ only in format would write one file; each
    /// keeps its source format in its name instead, so neither is lost and
    /// neither is refused.
    #[test]
    fn same_stem_inputs_of_two_formats_keep_their_source_format() {
        let jobs = jobs_from(&v(&["a.jpg", "a.PNG"]), Some("webp"), None).unwrap();
        assert_eq!(names(&outputs(&jobs)), ["a-jpg.webp", "a-png.webp"]);
        let jobs = jobs_from(&v(&["a.jpg", "b.png"]), Some("webp"), None).unwrap();
        assert_eq!(
            names(&outputs(&jobs)),
            ["a.webp", "b.webp"],
            "no clash, no rename"
        );
    }

    /// The clash is found through every spelling the check knows, so the
    /// rename happens there too.
    #[test]
    fn a_clash_spelled_differently_is_still_kept_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("sub").join("..").join("a.png");
        let jobs = jobs_from(&[a, b], Some("webp"), None).unwrap();
        assert_eq!(names(&outputs(&jobs)), ["a-jpg.webp", "a-png.webp"]);
    }

    /// `./a.png` and `a.png` are one file; comparing raw paths would let
    /// them slip past the collision check.
    #[test]
    fn relative_and_absolute_spellings_of_one_output_collide() {
        let e = jobs_from(&v(&["a.jpg", "./a.jpg"]), Some("webp"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("collide"), "{}", e.message);
    }

    /// On the case-insensitive filesystems macOS and Windows default to,
    /// `A.webp` and `a.webp` are the same file, so stems differing only in
    /// case are the same silent-overwrite race with extra steps.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn outputs_differing_only_by_case_collide_on_case_insensitive_platforms() {
        let e = jobs_from(&v(&["A.jpg", "a.jpg"]), Some("webp"), None).unwrap_err();
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
        let b = dir.path().join("sub").join("..").join("a.jpg");
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
        let b = dir.path().join("alias").join("x.jpg");
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
            &v(&["caf\u{e9}.jpg", "cafe\u{301}.jpg"]),
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

    // --- Skip already-converted files by format, not by location, so
    // re-running a batch stays idempotent without disabling in-place
    // conversion ----------------------------------------------------------

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
            upscale: false,
            quality: None,
            colors: None,
            fps: None,
            crf: None,
            max_size: None,
            strip_metadata: false,
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

    fn sized_to(size: &str, paths: Vec<PathBuf>, to: Option<&str>, outdir: Option<PathBuf>) -> Cli {
        let mut c = cli_for(paths, to, outdir);
        c.max_size = Some(convkit_core::size::parse(size).unwrap());
        c
    }

    fn sized(paths: Vec<PathBuf>, to: Option<&str>, outdir: Option<PathBuf>) -> Cli {
        sized_to("10mb", paths, to, outdir)
    }

    #[test]
    fn a_single_path_with_max_size_keeps_its_container_and_gains_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let jobs = plan_jobs(&sized(vec![clip.clone()], None, None)).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].output, dir.path().join("clip-10mb.mp4"));
        assert_eq!((jobs[0].from, jobs[0].to), (Format::Mp4, Format::Mp4));
    }

    #[test]
    fn an_explicit_output_equal_to_the_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let e = plan_jobs(&sized(vec![clip.clone(), clip], None, None)).unwrap_err();
        assert!(
            e.message.starts_with("output is the input"),
            "{}",
            e.message
        );
    }

    /// Only an existing output of the input's own format is the shape a
    /// glob of clips produces. A new name, another format (the ordinary `-y`
    /// rule), and the input itself (worded by its own refusal) all pass here.
    #[test]
    fn a_pair_is_refused_only_when_its_output_exists_in_the_inputs_format() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        let c = dir.path().join("c.mov");
        for f in [&a, &b, &c] {
            std::fs::write(f, b"x").unwrap();
        }
        let e = plan_jobs(&sized(vec![a.clone(), b.clone()], None, None)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e.message.contains("are both mp4 files"), "{}", e.message);

        let fresh = dir.path().join("small.mp4");
        let jobs = plan_jobs(&sized(vec![a.clone(), fresh.clone()], None, None)).unwrap();
        assert_eq!(jobs[0].output, fresh);
        let jobs = plan_jobs(&sized(vec![a.clone(), c.clone()], None, None)).unwrap();
        assert_eq!(jobs[0].output, c);
        let e = plan_jobs(&sized(vec![a.clone(), a], None, None)).unwrap_err();
        assert!(
            e.message.starts_with("output is the input"),
            "{}",
            e.message
        );
    }

    /// `--to png` would only move the refusal for a pair of images, so a
    /// pair in a format `--max-size` cannot size passes here, to be refused
    /// by the flag itself on that target.
    #[test]
    fn a_pair_that_cannot_be_sized_is_not_told_to_add_to() {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["png", "avi", "mp3"] {
            let a = dir.path().join(format!("a.{ext}"));
            let b = dir.path().join(format!("b.{ext}"));
            for f in [&a, &b] {
                std::fs::write(f, b"x").unwrap();
            }
            let jobs = plan_jobs(&sized(vec![a, b.clone()], None, None)).unwrap();
            assert_eq!(jobs[0].output, b, "{ext}");
            assert_eq!(jobs[0].from, jobs[0].to, "{ext}");
        }
    }

    /// The fix names the clips' own container, and the image merge form is
    /// left to its own refusal of `--max-size` on a pdf target.
    #[test]
    fn several_paths_under_max_size_without_to_name_the_fix() {
        let e = plan_jobs(&sized(v(&["a.webm", "b.webm", "c.webm"]), None, None)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.message.ends_with("; add --to webm to size each file"),
            "{}",
            e.message
        );
        let merge = plan_jobs(&sized(v(&["a.png", "b.png", "out.pdf"]), None, None)).unwrap();
        assert_eq!(merge[0].to, Format::Pdf);
    }

    #[test]
    fn the_ext_shorthand_is_derived_and_so_is_suffixed() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let jobs = plan_jobs(&sized(vec![clip, p(".mp4")], None, None)).unwrap();
        assert_eq!(jobs[0].output, dir.path().join("clip-10mb.mp4"));
    }

    /// A batch writes into a subfolder named after the size, under plain
    /// names, and creates it; a re-run never reads it back as input.
    #[test]
    fn a_sized_batch_writes_into_a_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mov");
        let jobs = plan_jobs(&sized(vec![a, b], Some("mp4"), None)).unwrap();
        assert_eq!(under(dir.path(), &jobs), ["10mb/a.mp4", "10mb/b.mp4"]);
        assert!(dir.path().join("10mb").is_dir());
    }

    /// `--dry-run` previews where the batch would write, and creates
    /// nothing.
    #[test]
    fn a_dry_run_batch_does_not_create_the_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        let mut cli = stripped(vec![dir.path().join("a.jpg")], Some("jpg"), None);
        cli.dry_run = true;
        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(under(dir.path(), &jobs), ["stripped/a.jpg"]);
        assert!(!dir.path().join("stripped").exists());
    }

    /// `-o` says where, so no subfolder is added.
    #[test]
    fn a_batch_with_an_outdir_writes_there() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let a = dir.path().join("a.heic");
        let jobs = plan_jobs(&stripped(vec![a], Some("jpg"), Some(out.clone()))).unwrap();
        assert_eq!(jobs[0].output, out.join("a.jpg"));
    }

    /// `a.mov -> a.mp4` would overwrite the input `a.mp4` while it is read,
    /// so the converted file keeps its source format in its name instead.
    #[test]
    fn an_output_landing_on_another_inputs_path_keeps_its_source_format() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("a.mov");
        let jobs = plan_jobs(&sized(vec![a, b], Some("mp4"), None)).unwrap();
        assert_eq!(under(dir.path(), &jobs), ["10mb/a.mp4", "10mb/a-mov.mp4"]);
    }

    #[test]
    fn a_single_path_whose_format_cannot_be_sized_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let e = plan_jobs(&sized(vec![dir.path().join("clip.avi")], None, None)).unwrap_err();
        assert!(e.message.contains("add --to mp4"), "{}", e.message);
    }

    #[test]
    fn with_an_outdir_elsewhere_the_name_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("small");
        let jobs = plan_jobs(&sized(
            vec![dir.path().join("clip.mp4")],
            None,
            Some(out.clone()),
        ))
        .unwrap();
        assert_eq!(jobs[0].output, out.join("clip.mp4"));
    }

    #[test]
    fn with_the_inputs_own_directory_as_outdir_the_name_is_suffixed() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let jobs = plan_jobs(&sized(vec![clip], None, Some(dir.path().to_path_buf()))).unwrap();
        assert_eq!(jobs[0].output, dir.path().join("clip-10mb.mp4"));
    }

    /// A lone sized copy beside its clip (`clip-80kb.mp4`) and a clip of
    /// that name in another format both land in the batch's subfolder under
    /// their own names.
    #[test]
    fn a_derived_name_that_another_job_also_writes_keeps_them_apart() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let other = dir.path().join("clip-80kb.mov");
        std::fs::write(&clip, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        let cli = sized_to("80kb", vec![clip, other], Some("mp4"), None);
        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(
            under(dir.path(), &jobs),
            ["80kb/clip.mp4", "80kb/clip-80kb.mp4"]
        );
    }

    /// An output named as another job's input is still refused: that job
    /// would be read while this one overwrites it. With `-o` into the inputs'
    /// own folder, `clip.mp4` is sized to `clip-80kb.mp4`, which a glob of
    /// `*.mp4` also took as an input.
    #[test]
    fn a_derived_name_that_is_another_jobs_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let previous = dir.path().join("clip-80kb.mp4");
        std::fs::write(&clip, b"x").unwrap();
        std::fs::write(&previous, b"x").unwrap();
        let here = Some(dir.path().to_path_buf());
        for order in [
            vec![clip.clone(), previous.clone()],
            vec![previous.clone(), clip.clone()],
        ] {
            let e = plan_jobs(&sized_to("80kb", order, Some("mp4"), here.clone())).unwrap_err();
            assert!(e.message.starts_with("outputs collide"), "{}", e.message);
            assert!(
                e.message.contains("would write") && e.message.ends_with("which is also an input"),
                "{}",
                e.message
            );
        }
    }

    /// `conv DIR --to mp4 --max-size 10mb` is the intended same-container
    /// case, so the directory keeps its mp4s -- but not a previous run's
    /// `a-10mb.mp4`, which would be re-encoded on every repeat. Without
    /// `--max-size` the same-format skip is unchanged.
    #[test]
    fn a_directory_under_max_size_keeps_same_format_files_but_not_its_own_results() {
        let dir = tempfile::tempdir().unwrap();
        let clips = dir.path().join("clips");
        std::fs::create_dir(&clips).unwrap();
        for name in ["a.mp4", "b.mov", "a-10mb.mp4"] {
            std::fs::write(clips.join(name), b"x").unwrap();
        }

        let jobs = plan_jobs(&sized(vec![clips.clone()], Some("mp4"), None)).unwrap();
        assert_eq!(
            names(&jobs.iter().map(|j| j.inputs[0].clone()).collect::<Vec<_>>()),
            vec!["a.mp4", "b.mov"],
            "{jobs:?}"
        );
        assert_eq!(under(&clips, &jobs), ["10mb/a.mp4", "10mb/b.mp4"]);

        let jobs = plan_jobs(&cli_for(vec![clips.clone()], Some("mp4"), None)).unwrap();
        assert_eq!(
            names(&jobs.iter().map(|j| j.inputs[0].clone()).collect::<Vec<_>>()),
            vec!["b.mov"],
            "without --max-size the same-format skip is unchanged: {jobs:?}"
        );
    }

    #[test]
    fn a_quoted_glob_matching_several_files_under_max_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.mp4", "b.mp4"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let e = plan_jobs(&sized(vec![dir.path().join("*.mp4")], None, None)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.message
                .ends_with("matched 2 files; add --to mp4 to size each one"),
            "{}",
            e.message
        );
    }

    #[test]
    fn the_glob_refusal_suggests_the_pattern_own_container_when_it_can_be_sized() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.mov", "b.mov"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let e = plan_jobs(&sized(vec![dir.path().join("*.mov")], None, None)).unwrap_err();
        assert!(
            e.message.ends_with("add --to mov to size each one"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_quoted_glob_matching_one_file_under_max_size_is_the_single_form() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clip.mp4"), b"x").unwrap();
        std::fs::write(dir.path().join("notes.md"), b"x").unwrap();
        let jobs = plan_jobs(&sized(vec![dir.path().join("*.mp4")], None, None)).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].inputs, vec![dir.path().join("clip.mp4")]);
        assert_eq!(jobs[0].output, dir.path().join("clip-10mb.mp4"));
    }

    #[test]
    fn the_single_form_suggests_to_mp4_only_where_it_would_work() {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["avi", "gif"] {
            let e = plan_jobs(&sized(
                vec![dir.path().join(format!("clip.{ext}"))],
                None,
                None,
            ))
            .unwrap_err();
            assert!(e.message.contains("add --to mp4"), "{ext}: {}", e.message);
        }
        for ext in ["mp3", "png", "pdf"] {
            let e = plan_jobs(&sized(
                vec![dir.path().join(format!("clip.{ext}"))],
                None,
                None,
            ))
            .unwrap_err();
            assert!(
                e.message.starts_with("--max-size applies to video; ")
                    && e.message
                        .ends_with(&format!("clip.{ext} is not a video file")),
                "{ext}: {}",
                e.message
            );
            assert!(!e.message.contains("--to"), "{ext}: {}", e.message);
        }
    }

    #[test]
    fn without_max_size_a_single_path_is_still_an_incomplete_invocation() {
        let e = plan_jobs(&cli_for(vec![p("clip.mp4")], None, None)).unwrap_err();
        assert!(
            e.message.contains("expected an input and an output"),
            "{}",
            e.message
        );
    }

    fn stripped(paths: Vec<PathBuf>, to: Option<&str>, outdir: Option<PathBuf>) -> Cli {
        let mut c = cli_for(paths, to, outdir);
        c.strip_metadata = true;
        c
    }

    #[test]
    fn a_lone_path_stripped_keeps_its_format_and_gains_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        for (name, out, format) in [
            ("IMG_0042.jpg", "IMG_0042-stripped.jpg", Format::Jpg),
            ("clip.MOV", "clip-stripped.MOV", Format::Mov),
            ("song.mp3", "song-stripped.mp3", Format::Mp3),
        ] {
            let jobs = plan_jobs(&stripped(vec![dir.path().join(name)], None, None)).unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].output, dir.path().join(out));
            assert_eq!((jobs[0].from, jobs[0].to), (format, format));
        }
    }

    /// conv cannot write heic, so the fix is a format it can.
    #[test]
    fn a_lone_path_that_cannot_be_stripped_in_place_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let e = plan_jobs(&stripped(vec![dir.path().join("IMG.heic")], None, None)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "conv cannot write heic; add --to jpg to strip it into a jpg"
        );
        let e = plan_jobs(&stripped(vec![dir.path().join("anim.gif")], None, None)).unwrap_err();
        assert_eq!(
            e.message,
            "conv cannot keep gif files as gif; add --to mp4 to strip them"
        );
        let e = plan_jobs(&stripped(vec![dir.path().join("a.docx")], None, None)).unwrap_err();
        assert!(
            e.message
                .starts_with("--strip-metadata does not apply to docx files"),
            "{}",
            e.message
        );
    }

    /// A glob of files conv cannot strip into their own format is told the
    /// format it can strip them into, which must be a real pair (gif -> jpg
    /// is not one), or that the flag does not apply.
    #[test]
    fn a_glob_conv_cannot_keep_is_told_a_format_it_can_strip_into() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.gif", "b.gif", "a.docx", "b.docx"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let e = plan_jobs(&stripped(vec![dir.path().join("*.gif")], None, None)).unwrap_err();
        assert!(
            e.message
                .ends_with("matched 2 files; add --to mp4 to strip each one"),
            "{}",
            e.message
        );
        let e = plan_jobs(&stripped(vec![dir.path().join("*.docx")], None, None)).unwrap_err();
        assert!(
            e.message.ends_with(
                "matched 2 files, and --strip-metadata does not apply to docx files: it \
                 covers image, video and audio conversions"
            ),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_stripped_output_equal_to_the_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("a.jpg");
        let e = plan_jobs(&stripped(vec![photo.clone(), photo], None, None)).unwrap_err();
        assert!(
            e.message.starts_with("output is the input"),
            "{}",
            e.message
        );
    }

    /// `conv *.jpg --strip-metadata` reaches conv as a list; two paths
    /// would replace the second photo with a stripped copy of the first.
    #[test]
    fn same_format_paths_without_to_are_told_to_add_it() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.jpg"), dir.path().join("b.jpg"));
        for f in [&a, &b] {
            std::fs::write(f, b"x").unwrap();
        }
        let e = plan_jobs(&stripped(vec![a.clone(), b.clone()], None, None)).unwrap_err();
        assert!(
            e.message
                .contains("are both jpg files; add --to jpg to strip each one, or remove"),
            "{}",
            e.message
        );
        assert!(
            e.message.ends_with("to write a stripped copy there"),
            "{}",
            e.message
        );
        let e = plan_jobs(&stripped(v(&["a.mp3", "b.mp3", "c.mp3"]), None, None)).unwrap_err();
        assert!(
            e.message.ends_with("; add --to mp3 to strip each file"),
            "{}",
            e.message
        );
        // A lone glob matching several files says the same.
        let pattern = dir.path().join("*.jpg");
        let e = plan_jobs(&stripped(vec![pattern], None, None)).unwrap_err();
        assert!(
            e.message.ends_with("add --to jpg to strip each one"),
            "{}",
            e.message
        );
    }

    /// A batch strips the files already in the target format too, but not
    /// what an earlier run wrote.
    #[test]
    fn a_stripped_folder_keeps_same_format_files_but_not_earlier_results() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.jpg", "a-stripped.jpg", "b.heic"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let jobs = plan_jobs(&stripped(vec![dir.path().to_path_buf()], Some("jpg"), None)).unwrap();
        assert_eq!(
            under(dir.path(), &jobs),
            ["stripped/a.jpg", "stripped/b.jpg"]
        );
    }

    /// A file already in the target format is kept for stripping only where
    /// the flag can keep it; a pdf among scans merged with `--to pdf` is
    /// skipped, as it is without the flag, rather than failed.
    #[test]
    fn a_stripped_folder_skips_target_files_it_cannot_keep() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.jpg", "b.png", "old.pdf"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let jobs = plan_jobs(&stripped(vec![dir.path().to_path_buf()], Some("pdf"), None)).unwrap();
        let inputs: Vec<String> = jobs
            .iter()
            .map(|j| {
                j.inputs[0]
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(inputs, ["a.jpg", "b.png"]);
    }

    /// A folder holding `IMG_1.heic` and `IMG_1.jpg`: both would be
    /// `stripped/IMG_1.jpg`, so the converted heic keeps its format in its
    /// name. Nothing is refused or skipped.
    #[test]
    fn a_heic_beside_a_jpg_of_the_same_name_keeps_its_format() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["IMG_1.heic", "IMG_1.jpg", "IMG_2.heic"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let jobs = plan_jobs(&stripped(vec![dir.path().to_path_buf()], Some("jpg"), None)).unwrap();
        assert_eq!(
            under(dir.path(), &jobs),
            [
                "stripped/IMG_1-heic.jpg",
                "stripped/IMG_1.jpg",
                "stripped/IMG_2.jpg"
            ]
        );
    }

    /// Both flags keep the format; the size names the file, as its rules
    /// (video only) are the narrower.
    #[test]
    fn with_max_size_too_the_size_names_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut cli = sized(vec![dir.path().join("clip.mp4")], None, None);
        cli.strip_metadata = true;
        let jobs = plan_jobs(&cli).unwrap();
        assert_eq!(jobs[0].output, dir.path().join("clip-10mb.mp4"));
    }
}
