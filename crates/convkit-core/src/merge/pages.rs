//! `conv merge` of PDFs and pictures into one PDF. Each run of pictures
//! between PDFs becomes a PDF of its own through Typst, and qpdf then joins
//! every file in order, as `crate::pdf` joins PDFs alone, so the PDFs'
//! pages come through untouched.
//!
//! A picture's page is the size of the PDF page next to it: the last page
//! of the PDF before it, or the first page of the PDF after it when the
//! pictures come first. The page is turned to the picture's orientation (a
//! landscape photo among portrait A4 pages gets a landscape A4 page), and
//! the picture is centred on it at its own size, shrunk to fit when it is
//! larger, never enlarged. A phone photo at 72 dpi would otherwise be a
//! 56-inch page among letter-sized ones, and the PDF would print and read
//! like two documents. A merge of pictures alone has no neighbour, so it
//! stays the photo merge, with each page the picture's own size.
//!
//! Typst reads JPG, PNG, WebP and GIF, and embeds a JPEG byte for byte.
//! Any other picture (HEIC, AVIF, TIFF, BMP, SVG, ICO) is first converted
//! by conv's own conversion: to JPEG from a photo format, to PNG from the
//! rest, which loses nothing.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{name, names_are};
use crate::error::{ConvError, ErrorCode, Result};
use crate::exec::{self, Event, Request};
use crate::pdf::range::join_and;
use crate::pdf::{self, path_arg, AskPassword, Lock, Password, PdfInfo, PdfOutcome, PdfPlan};
use crate::plan::{PlannedStep, Prepare, TextForm};
use crate::procutil::backend_command;
use crate::resolve::{ResolvedBackend, Resolver};
use crate::{layout, registry, Backend, Format, Kind, OutputMode};

/// A page's size in points, as it is shown: its crop box, turned by its
/// rotation.
pub type PageSize = (f64, f64);

/// The format a picture Typst cannot read is converted to first: JPEG for
/// a photo, which is lossy already, PNG for the rest, which loses nothing.
pub fn converted_format(f: Format) -> Option<Format> {
    if registry::TYPST_READS.contains(&f) {
        return None;
    }
    Some(match f {
        Format::Heic | Format::Heif | Format::Avif => Format::Jpg,
        _ => Format::Png,
    })
}

/// One run of pictures between PDFs, and the page size they take.
#[derive(Debug, Clone, PartialEq)]
pub struct PictureRun {
    /// Each picture's place among the inputs, and its path.
    pub pictures: Vec<(usize, PathBuf)>,
    /// The neighbouring page the pictures' pages are sized like.
    pub page: PageSize,
    /// Which input, and which of its pages, that is.
    pub beside: (PathBuf, u32),
}

/// A planned merge of PDFs and pictures.
#[derive(Debug, Clone, PartialEq)]
pub struct PagesPlan {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub runs: Vec<PictureRun>,
    /// Printed as `note` lines; JSON `warnings`.
    pub notes: Vec<String>,
    /// Each input's PDF info, `None` for a picture.
    infos: Vec<Option<PdfInfo>>,
    /// `--password`'s lock on the merged file.
    lock: Option<Lock>,
}

/// One Typst call, as it runs.
struct TypstCall {
    argv: Vec<String>,
    step: PlannedStep,
    /// `(user's picture, staged name, converted from)`.
    converted: Vec<(PathBuf, PathBuf, Format)>,
}

impl PagesPlan {
    /// Locks the merged file, as `PdfPlan::set_lock` does a PDF merge.
    pub fn set_lock(&mut self, lock: Lock) {
        self.lock = Some(lock);
    }

    /// Where run `r`'s PDF goes: inside `scratch` in a real run, beside the
    /// output for `--dry-run`.
    fn run_pdf(&self, r: usize, scratch: Option<&Path>) -> PathBuf {
        match scratch {
            Some(s) => s.join(format!("pictures-{}.pdf", r + 1)),
            None => self
                .output
                .with_extension(format!("convkit-pictures-{}.pdf", r + 1)),
        }
    }

    /// Typst's root, where its files are staged: the scratch folder, or
    /// the output's own folder for `--dry-run` (empty for the current one).
    fn root(&self, scratch: Option<&Path>) -> PathBuf {
        scratch.map(Path::to_path_buf).unwrap_or_else(|| {
            self.output
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf)
        })
    }

    fn typst_calls(&self, scratch: Option<&Path>) -> Vec<TypstCall> {
        let root = self.root(scratch);
        self.runs
            .iter()
            .enumerate()
            .map(|(r, run)| {
                let pdf_out = self.run_pdf(r, scratch);
                let mut text = String::from("#set page(margin: 0pt)\n");
                let mut prepare = Vec::new();
                let mut converted = Vec::new();
                for (index, path) in &run.pictures {
                    let from = Format::from_path(path);
                    let staged_as = match from.and_then(converted_format) {
                        Some(to) => {
                            let staged =
                                layout::staged_name(*index, Path::new(&format!("x.{}", to.ext())));
                            converted.push((path.clone(), root.join(&staged), to));
                            staged
                        }
                        None => layout::staged_name(*index, path),
                    };
                    text.push_str(&picture_page(&staged_as, run.page));
                    text.push('\n');
                    prepare.push(Prepare::Stage {
                        from: path.clone(),
                        to: root.join(&staged_as),
                        strip: false,
                    });
                }
                let layout_file = layout::layout_path(&pdf_out);
                prepare.push(Prepare::Write {
                    path: layout_file.clone(),
                    contents: text,
                });
                let root_arg = if root.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    root.clone()
                };
                // On Windows, every path in `\\?\` form when one is long, as
                // `exec` gives a Typst step's: Typst does not open a path past
                // 260 characters otherwise.
                let verbatim = scratch.is_some()
                    && [&root_arg, &layout_file, &pdf_out]
                        .iter()
                        .any(|p| crate::winpath::is_long(&p.to_string_lossy()));
                let shown = |p: &Path| {
                    let arg = path_arg(p);
                    if verbatim {
                        crate::winpath::to_verbatim(&arg).unwrap_or(arg)
                    } else {
                        arg
                    }
                };
                let argv = vec![
                    "compile".to_string(),
                    "--root".to_string(),
                    shown(&root_arg),
                    "--ignore-system-fonts".to_string(),
                    "--diagnostic-format".to_string(),
                    "short".to_string(),
                    shown(&layout_file),
                    shown(&pdf_out),
                ];
                TypstCall {
                    argv: argv.clone(),
                    step: PlannedStep {
                        backend: Backend::Typst,
                        program: "typst".into(),
                        argv,
                        output_mode: OutputMode::Path,
                        output: pdf_out,
                        intermediate_ext: None,
                        path_args: Vec::new(),
                        prepare,
                        stdin: None,
                    },
                    converted,
                }
            })
            .collect()
    }

    /// The qpdf join, reading the pictures' PDFs from `scratch` (or from
    /// beside the output, for `--dry-run`).
    fn join_plan(&self, scratch: Option<&Path>) -> Result<PdfPlan> {
        let mut infos = Vec::new();
        let mut run = 0;
        for (i, info) in self.infos.iter().enumerate() {
            match info {
                Some(info) => infos.push(info.clone()),
                // A run's PDF stands where its first picture was.
                None if self.runs.get(run).is_some_and(|r| r.pictures[0].0 == i) => {
                    infos.push(PdfInfo {
                        path: self.run_pdf(run, scratch),
                        pages: self.runs[run].pictures.len() as u32,
                        has_bookmarks: false,
                        bookmarked_pages: Vec::new(),
                        encrypted: false,
                        restricted: false,
                        password: None,
                        damaged: false,
                    });
                    run += 1;
                }
                None => {}
            }
        }
        let mut join = pdf::plan::merge_plan(&infos, &self.output)?;
        if let Some(lock) = &self.lock {
            join.set_lock(lock.clone());
        }
        Ok(join)
    }

    /// What `--dry-run` prints: each picture conversion's commands, each
    /// Typst call, then qpdf's join, with the program each runs.
    pub fn commands(&self) -> Result<Vec<(Backend, Vec<String>)>> {
        let mut out = Vec::new();
        for call in self.typst_calls(None) {
            for (picture, staged, to) in &call.converted {
                let from = Format::from_path(picture).unwrap_or(Format::Png);
                let conversion = crate::plan::build(
                    from,
                    *to,
                    std::slice::from_ref(picture),
                    staged,
                    None,
                    None,
                )?;
                out.extend(conversion.steps.into_iter().map(|s| (s.backend, s.argv)));
            }
            out.push((Backend::Typst, call.argv));
        }
        let join = self.join_plan(None)?;
        out.extend(join.commands().into_iter().map(|a| (Backend::Qpdf, a)));
        Ok(out)
    }

    /// Each Typst layout, by where `--dry-run` says it goes.
    pub fn layouts(&self) -> Vec<(PathBuf, String)> {
        self.typst_calls(None)
            .into_iter()
            .filter_map(|c| match c.step.prepare.last() {
                Some(Prepare::Write { path, contents }) => Some((path.clone(), contents.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every note a run prints: the join's own and the pictures'.
    pub fn all_notes(&self) -> Result<Vec<String>> {
        let mut notes = self.join_plan(None)?.notes;
        notes.extend(self.notes.iter().cloned());
        Ok(notes)
    }

    /// The join's warnings (inputs qpdf repaired).
    pub fn warnings(&self) -> Result<Vec<String>> {
        Ok(self.join_plan(None)?.warnings)
    }

    /// How many pages the merged file has.
    pub fn pages(&self) -> u32 {
        let pdfs: u32 = self.infos.iter().flatten().map(|i| i.pages).sum();
        let pictures: usize = self.runs.iter().map(|r| r.pictures.len()).sum();
        pdfs + pictures as u32
    }
}

/// `n` points, as Typst reads a length.
fn pt(n: f64) -> String {
    let s = format!("{n:.3}");
    format!("{}pt", s.trim_end_matches('0').trim_end_matches('.'))
}

/// One layout line: the picture staged as `staged`, on a page `page`'s
/// size turned to the picture's orientation, centred at its own size, or
/// shrunk to fit when larger. One line, so a Typst error's line still
/// leads to the picture (`layout::name_inputs`). Typst measures the
/// picture as it shows it: upright by its EXIF orientation, sized by its
/// DPI.
fn picture_page(staged: &str, (w, h): PageSize) -> String {
    let (long, short) = (pt(w.max(h)), pt(w.min(h)));
    let size = if (w - h).abs() < 0.01 {
        format!("({long}, {long})")
    } else {
        format!(
            "if s.width > s.height {{ ({long}, {short}) }} else if s.width < s.height \
             {{ ({short}, {long}) }} else {{ ({}, {}) }}",
            pt(w),
            pt(h)
        )
    };
    format!(
        "#context {{ let s = measure(image(\"/{staged}\")); let (w, h) = {size}; \
         let k = calc.min(1, w / s.width, h / s.height); \
         page(width: w, height: h, align(center + horizon, image(\"/{staged}\", width: s.width * k))) }}"
    )
}

/// The arguments that read every page and object of `input`, for its page
/// sizes.
fn boxes_args(input: &Path) -> Vec<String> {
    vec![
        "--json".to_string(),
        "--json-key=pages".to_string(),
        "--json-key=qpdf".to_string(),
        path_arg(input),
    ]
}

/// The sizes of `pages` (numbered from 1) of `input`, opened with
/// `password` when it needs one.
pub fn page_sizes(
    qpdf: &Path,
    input: &Path,
    pages: &[u32],
    password: Option<&Password>,
) -> Result<Vec<PageSize>> {
    let mut argv: Vec<String> = password
        .map(|p| pdf::read::password_arg(p, true))
        .into_iter()
        .collect();
    argv.extend(boxes_args(input));
    let out = pdf::qpdf_output(qpdf, &argv, password.is_some(), None).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("failed to run {}: {e}", qpdf.display()),
        )
    })?;
    let unreadable = || ConvError {
        backend: Some(Backend::Qpdf),
        ..ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not read the page sizes of {}", name(input)),
        )
    };
    if !matches!(out.status.code(), Some(0) | Some(3)) {
        return Err(unreadable());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| unreadable())?;
    pages
        .iter()
        .map(|&p| page_size(&v, p).ok_or_else(unreadable))
        .collect()
}

/// Page `page`'s size in a `qpdf --json` report with its `pages` and
/// `qpdf` keys: the crop box (inherited, else the media box) within the
/// media box, times `UserUnit`, turned by `Rotate`.
pub fn page_size(report: &Value, page: u32) -> Option<PageSize> {
    let objects = report["qpdf"].get(1)?.as_object()?;
    let lookup = |r: &str| objects.get(&format!("obj:{r}")).map(|o| &o["value"]);
    // A value, or the object a reference to one names.
    let resolve = |v: &Value| -> Option<Value> {
        match v.as_str() {
            Some(s) if s.ends_with(" R") => lookup(s).cloned(),
            _ => Some(v.clone()),
        }
    };
    let page_ref = report["pages"].get(usize::try_from(page).ok()?.checked_sub(1)?)?["object"]
        .as_str()?
        .to_string();
    let page_dict = lookup(&page_ref)?.clone();
    // An attribute the page or a node above it sets.
    let inherited = |key: &str| -> Option<Value> {
        let mut node = page_dict.clone();
        for _ in 0..64 {
            if let Some(v) = node.get(key) {
                return resolve(v);
            }
            let parent = node.get("/Parent")?.as_str()?.to_string();
            node = lookup(&parent)?.clone();
        }
        None
    };
    let rect = |v: Value| -> Option<[f64; 4]> {
        let a = v.as_array()?;
        let mut r = [0.0; 4];
        for (i, n) in a.iter().take(4).enumerate() {
            r[i] = resolve(n)?.as_f64()?;
        }
        (a.len() >= 4).then_some(r)
    };
    let media = inherited("/MediaBox")
        .and_then(rect)
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let cropped = inherited("/CropBox").and_then(rect).unwrap_or(media);
    let (mx1, mx2) = (media[0].min(media[2]), media[0].max(media[2]));
    let (my1, my2) = (media[1].min(media[3]), media[1].max(media[3]));
    let (cx1, cx2) = (cropped[0].min(cropped[2]), cropped[0].max(cropped[2]));
    let (cy1, cy2) = (cropped[1].min(cropped[3]), cropped[1].max(cropped[3]));
    let mut w = cx2.min(mx2) - cx1.max(mx1);
    let mut h = cy2.min(my2) - cy1.max(my1);
    if w <= 0.0 || h <= 0.0 {
        (w, h) = (mx2 - mx1, my2 - my1);
    }
    let unit = page_dict
        .get("/UserUnit")
        .and_then(resolve)
        .and_then(|u| u.as_f64())
        .filter(|u| *u > 0.0)
        .unwrap_or(1.0);
    let turn = inherited("/Rotate")
        .and_then(|r| r.as_i64())
        .unwrap_or(0)
        .rem_euclid(360);
    let (w, h) = (w * unit, h * unit);
    (w > 0.0 && h > 0.0).then_some(if turn % 180 == 90 { (h, w) } else { (w, h) })
}

/// Whether `p` goes into a PDF merge as a picture.
pub fn is_picture(p: &Path) -> bool {
    Format::from_path(p).is_some_and(|f| f.kind() == Kind::Image)
}

/// Plans a merge of `inputs`, PDFs and pictures in any order with at least
/// one of each, into `output`. `infos` holds each PDF's info, `None` for a
/// picture; `sizes(input, page)` gives the size of a PDF's page.
pub fn plan(
    inputs: &[PathBuf],
    infos: Vec<Option<PdfInfo>>,
    output: &Path,
    sizes: &mut dyn FnMut(&Path, u32) -> Result<PageSize>,
) -> Result<PagesPlan> {
    debug_assert_eq!(inputs.len(), infos.len());
    if !infos.iter().any(Option::is_some) || !infos.iter().any(Option::is_none) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "a merge of pages takes PDFs and pictures together",
        ));
    }
    let mut runs: Vec<PictureRun> = Vec::new();
    let mut i = 0;
    while i < inputs.len() {
        if infos[i].is_some() {
            i += 1;
            continue;
        }
        let start = i;
        while i < inputs.len() && infos[i].is_none() {
            i += 1;
        }
        // The page beside the run: the last of the PDF before, else the
        // first of the PDF after.
        let before = (0..start).rev().find_map(|j| {
            infos[j]
                .as_ref()
                .filter(|info| info.pages > 0)
                .map(|info| (j, info.pages))
        });
        let after = (i..inputs.len()).find_map(|j| {
            infos[j]
                .as_ref()
                .filter(|info| info.pages > 0)
                .map(|_| (j, 1))
        });
        let Some((j, page)) = before.or(after) else {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "the PDFs in this merge have no pages",
            ));
        };
        runs.push(PictureRun {
            pictures: (start..i).map(|k| (k, inputs[k].clone())).collect(),
            page: sizes(&inputs[j], page)?,
            beside: (inputs[j].clone(), page),
        });
    }

    let out_name = name(output);
    let mut notes = Vec::new();
    let located: Vec<String> = inputs
        .iter()
        .filter(|p| Format::from_path(p) == Some(Format::Jpg))
        .filter(|p| crate::jpeg::file_has_location(p))
        .map(|p| name(p))
        .fold(Vec::new(), |mut v, n| {
            if !v.contains(&n) {
                v.push(n);
            }
            v
        });
    if let Some(note) = location_note(&located, &out_name) {
        notes.push(note);
    }
    if inputs
        .iter()
        .any(|p| Format::from_path(p) == Some(Format::Gif))
    {
        notes.push(registry::GIF_FIRST_FRAME_NOTE.to_string());
    }
    Ok(PagesPlan {
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        runs,
        notes,
        infos,
        lock: None,
    })
}

/// Said when a JPEG that records where it was taken goes into the PDF as
/// it is, GPS and all.
fn location_note(located: &[String], out_name: &str) -> Option<String> {
    let first = located.first()?;
    let (names, _) = names_are(located);
    let verb = if located.len() == 1 {
        "records"
    } else {
        "record"
    };
    Some(format!(
        "{names} {verb} where {} taken, and {out_name} keeps it; conv {first} --strip-metadata \
         makes a copy without it.",
        if located.len() == 1 {
            "it was"
        } else {
            "they were"
        }
    ))
}

/// Reads every PDF among `inputs` with qpdf (each distinct path once,
/// opening a locked one with what `ask` gives), and the size of each page
/// a picture is placed beside, and plans the merge.
pub fn plan_merge(
    qpdf: &Path,
    inputs: &[PathBuf],
    output: &Path,
    ask: &mut AskPassword,
) -> Result<PagesPlan> {
    for input in inputs {
        if !input.is_file() {
            return Err(ConvError::new(
                ErrorCode::InputNotFound,
                format!("input not found: {}", input.display()),
            ));
        }
    }
    let mut seen: Vec<PdfInfo> = Vec::new();
    let mut infos = Vec::with_capacity(inputs.len());
    for input in inputs {
        if is_picture(input) {
            infos.push(None);
            continue;
        }
        let info = match seen.iter().find(|i| &i.path == input) {
            Some(i) => i.clone(),
            None => {
                let i = pdf::read::open(qpdf, input, ask)?;
                seen.push(i.clone());
                i
            }
        };
        infos.push(Some(info));
    }
    plan(inputs, infos, output, &mut |p, page| {
        let password = seen
            .iter()
            .find(|i| i.path == p)
            .and_then(|i| i.password.as_ref());
        Ok(page_sizes(qpdf, p, &[page], password)?[0])
    })
}

/// Runs `plan`: converts the pictures Typst cannot read, makes each run's
/// PDF with Typst in a scratch folder beside the output, then joins
/// everything with qpdf (`pdf::run`, which refuses an existing output
/// unless `overwrite`). The scratch folder is removed on every exit path.
pub fn run(
    plan: &PagesPlan,
    resolver: &Resolver,
    typst: &ResolvedBackend,
    qpdf: &ResolvedBackend,
    overwrite: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<PdfOutcome> {
    // Refused before any picture is placed, as `pdf::run` would after.
    crate::winpath::check_output_name(&plan.output)?;
    if plan.output.exists() && !overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", plan.output.display()),
        ));
    }
    let dest_dir = plan
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = exec::make_scratch_dir(dest_dir)?;
    let _guard = exec::ScratchGuard::new(scratch.clone());

    let mut notes = Vec::new();
    let mut located_after = Vec::new();
    for call in plan.typst_calls(Some(&scratch)) {
        for (picture, staged, to) in &call.converted {
            let from = Format::from_path(picture).unwrap_or(Format::Png);
            let req = Request {
                from,
                to: *to,
                inputs: vec![picture.clone()],
                output: staged.clone(),
                overwrite: true,
                tuning: crate::Tuning::default(),
                allow_extreme: false,
            };
            exec::run(&req, resolver, on_event).map_err(|mut e| {
                e.message = format!("{}: {}", name(picture), e.message);
                e
            })?;
            if *to == Format::Jpg && crate::jpeg::file_has_location(staged) {
                located_after.push(name(picture));
            }
        }
        on_event(Event::StepStarted {
            index: 0,
            total: 1,
            backend: Backend::Typst,
        });
        for prepare in &call.step.prepare {
            match prepare {
                Prepare::Write { path, contents } => {
                    std::fs::write(path, contents)
                        .map_err(|e| ConvError::new(ErrorCode::ConversionFailed, e.to_string()))?;
                }
                Prepare::Stage { from, to, .. } => {
                    if !call.converted.iter().any(|(p, _, _)| p == from) {
                        exec::stage_file(from, to, false, exec::hard_link)?;
                    }
                }
                // A merge stages pictures, never text, but runs one the
                // way exec does all the same.
                Prepare::Text {
                    from,
                    to,
                    form,
                    source,
                } => match form {
                    TextForm::Page { title } => crate::text::write_page(from, to, title, source)?,
                    TextForm::Speech => crate::text::write_speakable(from, to, source)?,
                },
            }
        }
        on_event(Event::StepSpawned {
            index: 0,
            program: typst.path.clone(),
            argv: call.argv.clone(),
        });
        let out = backend_command(&typst.path)
            .args(&call.argv)
            .output()
            .map_err(|e| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("failed to run {}: {e}", typst.path.display()),
                )
            })?;
        let report = layout::name_inputs(&String::from_utf8_lossy(&out.stderr), &call.step);
        if !report.trim().is_empty() {
            on_event(Event::StepReport {
                index: 0,
                backend: Backend::Typst,
                report: exec::tail_str(&report, 16 * 1024).to_string(),
            });
        }
        let made = std::fs::metadata(&call.step.output).is_ok_and(|m| m.len() > 0);
        if !out.status.success() || !made {
            let salient = exec::classify_backend_noise(Backend::Typst, &report);
            let detail = salient
                .first()
                .cloned()
                .or_else(|| {
                    report
                        .lines()
                        .rfind(|l| !l.trim().is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            return Err(ConvError {
                backend: Some(Backend::Typst),
                ..ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("typst failed ({}): {}", out.status, detail.trim()),
                )
            });
        }
        on_event(Event::StepFinished { index: 0 });
    }
    if let Some(note) = location_note(&located_after, &name(&plan.output)) {
        notes.push(note);
    }

    let join = plan.join_plan(Some(&scratch))?;
    let mut outcome = pdf::run(&join, qpdf, overwrite, on_event)?;
    outcome.notes.extend(plan.notes.iter().cloned());
    outcome.notes.extend(notes);
    Ok(outcome)
}

/// The names of `inputs` that are pictures, for a message.
pub fn picture_names(inputs: &[PathBuf]) -> String {
    let names: Vec<String> = inputs
        .iter()
        .filter(|p| is_picture(p))
        .map(|p| name(p))
        .collect();
    join_and(&names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(path: &str, pages: u32) -> Option<PdfInfo> {
        Some(PdfInfo {
            path: PathBuf::from(path),
            pages,
            has_bookmarks: false,
            bookmarked_pages: Vec::new(),
            encrypted: false,
            restricted: false,
            password: None,
            damaged: false,
        })
    }

    fn p(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    /// Letter for a.pdf's pages, A4 for b.pdf's.
    fn sizes(input: &Path, _page: u32) -> Result<PageSize> {
        Ok(if input.ends_with("a.pdf") {
            (612.0, 792.0)
        } else {
            (595.276, 841.89)
        })
    }

    #[test]
    fn pictures_take_the_page_before_them_or_else_the_one_after() {
        let inputs = p(&["cover.jpg", "a.pdf", "x.png", "y.heic", "b.pdf", "z.jpg"]);
        let infos = vec![None, info("a.pdf", 3), None, None, info("b.pdf", 2), None];
        let mut asked = Vec::new();
        let plan = plan(&inputs, infos, Path::new("out.pdf"), &mut |i, page| {
            asked.push((i.to_path_buf(), page));
            sizes(i, page)
        })
        .unwrap();
        assert_eq!(plan.runs.len(), 3);
        assert_eq!(plan.runs[0].beside, (PathBuf::from("a.pdf"), 1));
        assert_eq!(plan.runs[1].beside, (PathBuf::from("a.pdf"), 3));
        assert_eq!(plan.runs[2].beside, (PathBuf::from("b.pdf"), 2));
        assert_eq!(
            plan.runs[1].pictures,
            vec![(2, "x.png".into()), (3, "y.heic".into())]
        );
        assert_eq!(plan.pages(), 3 + 2 + 4);
        assert_eq!(asked.len(), 3);
    }

    #[test]
    fn the_join_puts_each_runs_pdf_where_its_pictures_were() {
        let inputs = p(&["cover.jpg", "a.pdf", "x.png", "y.png", "b.pdf"]);
        let infos = vec![None, info("a.pdf", 3), None, None, info("b.pdf", 2)];
        let plan = plan(&inputs, infos, Path::new("out.pdf"), &mut sizes).unwrap();
        let cmds = plan.commands().unwrap();
        let (backends, argvs): (Vec<Backend>, Vec<Vec<String>>) = cmds.into_iter().unzip();
        assert_eq!(backends, [Backend::Typst, Backend::Typst, Backend::Qpdf]);
        assert_eq!(
            argvs[0],
            [
                "compile",
                "--root",
                ".",
                "--ignore-system-fonts",
                "--diagnostic-format",
                "short",
                "out.convkit-pictures-1.convkit-layout.typ",
                "out.convkit-pictures-1.pdf"
            ]
        );
        assert_eq!(
            argvs[2],
            [
                "--decrypt",
                "out.convkit-pictures-1.pdf",
                "--pages",
                ".",
                "a.pdf",
                "out.convkit-pictures-2.pdf",
                "b.pdf",
                "--",
                "out.pdf"
            ]
        );
    }

    #[test]
    fn a_layout_line_per_picture_turns_the_page_to_the_picture() {
        let inputs = p(&["a.pdf", "x.png", "y.jpg"]);
        let plan = plan(
            &inputs,
            vec![info("a.pdf", 1), None, None],
            Path::new("out.pdf"),
            &mut sizes,
        )
        .unwrap();
        let (path, text) = plan.layouts().pop().unwrap();
        assert_eq!(
            path,
            PathBuf::from("out.convkit-pictures-1.convkit-layout.typ")
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#set page(margin: 0pt)");
        assert_eq!(lines.len(), 3, "a picture on each line after the first");
        assert_eq!(
            lines[1],
            "#context { let s = measure(image(\"/convkit-in-0002.png\")); \
             let (w, h) = if s.width > s.height { (792pt, 612pt) } else if s.width < s.height \
             { (612pt, 792pt) } else { (612pt, 792pt) }; \
             let k = calc.min(1, w / s.width, h / s.height); \
             page(width: w, height: h, align(center + horizon, image(\"/convkit-in-0002.png\", \
             width: s.width * k))) }"
        );
        assert!(lines[2].contains("/convkit-in-0003.jpg"));
    }

    #[test]
    fn a_picture_typst_cannot_read_is_converted_first() {
        assert_eq!(converted_format(Format::Heic), Some(Format::Jpg));
        assert_eq!(converted_format(Format::Tiff), Some(Format::Png));
        assert_eq!(converted_format(Format::Svg), Some(Format::Png));
        assert_eq!(converted_format(Format::Jpg), None);
        assert_eq!(converted_format(Format::Gif), None);
        let inputs = p(&["a.pdf", "IMG_1.HEIC"]);
        let plan = plan(
            &inputs,
            vec![info("a.pdf", 1), None],
            Path::new("out.pdf"),
            &mut sizes,
        )
        .unwrap();
        let (_, text) = plan.layouts().pop().unwrap();
        assert!(text.contains("/convkit-in-0002.jpg"), "{text}");
        let cmds = plan.commands().unwrap();
        assert_eq!(cmds[0].0, Backend::Magick, "{cmds:?}");
        assert_eq!(cmds[0].1.last().unwrap(), "convkit-in-0002.jpg");
    }

    #[test]
    fn a_square_page_stays_square() {
        let line = picture_page("convkit-in-0001.png", (500.0, 500.0));
        assert!(line.contains("let (w, h) = (500pt, 500pt);"), "{line}");
    }

    #[test]
    fn a_merge_without_both_kinds_is_not_this_one() {
        let e = plan(
            &p(&["a.pdf"]),
            vec![info("a.pdf", 1)],
            Path::new("o.pdf"),
            &mut sizes,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
    }

    #[test]
    fn gif_pages_get_the_first_frame_note() {
        let plan = plan(
            &p(&["a.pdf", "anim.gif"]),
            vec![info("a.pdf", 1), None],
            Path::new("out.pdf"),
            &mut sizes,
        )
        .unwrap();
        assert_eq!(plan.notes, [registry::GIF_FIRST_FRAME_NOTE]);
    }

    #[test]
    fn the_location_note_names_the_photos() {
        assert_eq!(
            location_note(&["a.jpg".into()], "out.pdf").unwrap(),
            "a.jpg records where it was taken, and out.pdf keeps it; conv a.jpg \
             --strip-metadata makes a copy without it."
        );
        assert!(location_note(&["a.jpg".into(), "b.jpg".into()], "out.pdf")
            .unwrap()
            .starts_with("a.jpg and b.jpg record where they were taken"));
        assert!(location_note(&[], "out.pdf").is_none());
    }

    fn report(objects: Value) -> Value {
        serde_json::json!({
            "pages": [{"object": "3 0 R"}, {"object": "4 0 R"}],
            "qpdf": [{"jsonversion": 2}, objects]
        })
    }

    #[test]
    fn a_page_size_is_inherited_cropped_and_turned() {
        let r = report(serde_json::json!({
            "obj:1 0 R": {"value": {"/Type": "/Pages", "/MediaBox": [0, 0, 612, 792], "/Rotate": 90}},
            "obj:3 0 R": {"value": {"/Type": "/Page", "/Parent": "1 0 R"}},
            "obj:4 0 R": {"value": {"/Type": "/Page", "/Parent": "1 0 R", "/Rotate": 0,
                                     "/MediaBox": "5 0 R", "/CropBox": [10, 10, 410, 310.5]}},
            "obj:5 0 R": {"value": [0, 0, 595.276, 841.89]}
        }));
        assert_eq!(page_size(&r, 1), Some((792.0, 612.0)));
        assert_eq!(page_size(&r, 2), Some((400.0, 300.5)));
        assert_eq!(page_size(&r, 3), None);
    }

    #[test]
    fn a_user_unit_scales_the_page() {
        let r = report(serde_json::json!({
            "obj:3 0 R": {"value": {"/MediaBox": [0, 0, 100, 200], "/UserUnit": 2}},
            "obj:4 0 R": {"value": {}}
        }));
        assert_eq!(page_size(&r, 1), Some((200.0, 400.0)));
        // No box at all is letter, as viewers take it.
        assert_eq!(page_size(&r, 2), Some((612.0, 792.0)));
    }

    #[test]
    fn lengths_are_written_short() {
        assert_eq!(pt(612.0), "612pt");
        assert_eq!(pt(595.276), "595.276pt");
        assert_eq!(pt(841.8898), "841.89pt");
    }
}
