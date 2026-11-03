//! `--compress`: a PDF made smaller with the same look.
//!
//! Almost all of a large PDF is its images, and qpdf, which convkit
//! manages, only ever rewrites losslessly. So the images are done here, in
//! process, between two qpdf passes:
//!
//! 1. qpdf repairs the file, takes every object out of object streams and
//!    removes owner-password protection, into a scratch copy;
//! 2. lopdf reads that copy; a walk over every page finds each image and
//!    the resolution it is drawn at (`walk.rs`), and each eligible image is
//!    downsampled or re-encoded by the level's rules (`image.rs`);
//! 3. qpdf writes the result from the scratch copy, replacing only the
//!    images that changed (`--update-from-json`), with object streams and
//!    Flate at level 9 and the input's encryption copied back, then checks
//!    it;
//! 4. a result that is not smaller is replaced by a copy of the input.
//!
//! Text, fonts, links, bookmarks, form fields, tags and metadata are never
//! touched: conv writes no object but the images (and, for a file qpdf had
//! to repair, a trailer qpdf left without `/Size`). Like the rest of
//! convkit-core, it never prints.

mod codec;
mod image;
mod notes;
mod update;
mod walk;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lopdf::{Document, Object, ObjectId};
use serde::Serialize;

use super::{display_name, path_arg, qpdf_reason, read};
use crate::error::{ConvError, ErrorCode, Remediation, Result};
use crate::exec::{self, BackendOutput, Event, Outcome, Request};
use crate::plan::{ConversionPlan, PlannedStep};
use crate::procutil::backend_command;
use crate::resolve::ResolvedBackend;
use crate::size::UnitFamily;
use crate::sized::{SizedKind, SizingReport, Strategy};
use crate::{Backend, Format, OutputMode, Resolver, Tuning};

pub use image::Skip;

use image::{Candidate, Change, Replacement, Surveyed};

/// How hard a pass squeezes images. Every level keeps the document's
/// structure; they differ only in the images.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Level {
    pub n: u8,
    /// Images drawn above this resolution are downsampled ...
    pub above_dpi: f64,
    /// ... to this one.
    pub to_dpi: f64,
    /// JPEG quality for everything a pass writes as JPEG.
    pub quality: u8,
    /// A JPEG whose estimated quality is above this is re-encoded at
    /// `quality`.
    pub requant_above: f64,
    /// No image's long side stays above this many pixels.
    pub max_side: Option<u32>,
}

/// The ladder. L0 changes no image (qpdf's lossless rewrite alone); L1 is
/// `--compress`; L2 is what `--compress` measures for its suggestion;
/// `--max-size` climbs from L0 (from L1 with `--compress`) and stops at the
/// first level under its target, asking before L4. Calibrated on real
/// scans, decks and papers: see "Compress PDF" in
/// docs/defaults-calibration.md.
pub(crate) const LEVELS: [Level; 6] = [
    Level {
        n: 0,
        above_dpi: f64::INFINITY,
        to_dpi: f64::INFINITY,
        quality: 100,
        requant_above: f64::INFINITY,
        max_side: None,
    },
    // The design's spike values: above 225 dpi nothing more shows at
    // normal zoom, and 150 dpi keeps receipts and handwriting readable at
    // 220 dpi zoom. Measured again on the spike's corpus in
    // docs/defaults-calibration.md.
    Level {
        n: 1,
        above_dpi: 225.0,
        to_dpi: 150.0,
        quality: 80,
        requant_above: 85.0,
        max_side: None,
    },
    // A typical office scan is 200 dpi at about q75, which L1 keeps: here it
    // goes to 150 dpi. 165, not 150, so an image barely above is not
    // resampled for nothing.
    Level {
        n: 2,
        above_dpi: 165.0,
        to_dpi: 150.0,
        quality: 65,
        requant_above: 70.0,
        max_side: None,
    },
    Level {
        n: 3,
        above_dpi: 132.0,
        to_dpi: 120.0,
        quality: 60,
        requant_above: 65.0,
        max_side: None,
    },
    // The extreme levels: print gets hard to read.
    Level {
        n: 4,
        above_dpi: 106.0,
        to_dpi: 96.0,
        quality: 50,
        requant_above: 55.0,
        max_side: Some(2400),
    },
    Level {
        n: 5,
        above_dpi: 80.0,
        to_dpi: 72.0,
        quality: 40,
        requant_above: 45.0,
        max_side: Some(1600),
    },
];

/// The first level `--max-size` asks about.
const EXTREME: usize = 4;

/// The `--compress` level.
const COMPRESS: usize = 1;
/// The level `--compress` measures when it saved little.
const SUGGEST: usize = 2;
/// `--compress` saving less than this share, per cent, measures L2 ...
const SMALL_SAVING_PERCENT: u64 = 10;
/// ... and suggests it when it comes out at or under this share of the
/// input, per cent.
const SUGGEST_AT_MOST_PERCENT: u64 = 75;

/// What `--json`'s `sizing` adds for a PDF.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PdfSizing {
    /// The level that made the file: 0 changed no image, and `None` means
    /// the file is a copy of the input.
    pub level: Option<u8>,
    pub source_bytes: u64,
    pub images_downsampled: u32,
    pub images_reencoded: u32,
    /// Images left as they are, by reason.
    pub images_skipped: BTreeMap<Skip, u32>,
}

/// Whether this conversion is one for the PDF engine: `--compress`, or
/// `--max-size` with a PDF on either side (the engine refuses any other
/// format with the two steps that do it).
pub fn applies(tuning: &Tuning, from: Format, to: Format) -> bool {
    tuning.compress || (tuning.max_size.is_some() && (from == Format::Pdf || to == Format::Pdf))
}

/// The flag a refusal names: `--max-size 5mb`, else `--compress`.
fn asked(tuning: &Tuning) -> String {
    match &tuning.max_size {
        Some(m) => format!("--max-size {}", m.spelling),
        None => "--compress".to_string(),
    }
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// The flags `--compress` cannot be combined with, by name.
fn other_knobs(t: &Tuning) -> Vec<&'static str> {
    [
        (t.resize.is_some(), "--resize"),
        (t.upscale, "--upscale"),
        (t.quality.is_some(), "--quality"),
        (t.colors.is_some(), "--colors"),
        (t.fps.is_some(), "--fps"),
        (t.crf.is_some(), "--crf"),
        (t.strip_metadata, "--strip-metadata"),
        (t.range.is_some(), "--start/--end/--duration"),
        (t.speed.is_some(), "--speed"),
        (t.rotate.is_some(), "--rotate"),
        (t.flip.is_some(), "--flip"),
        (t.crop.is_some(), "--crop"),
        (t.audio.is_some(), "--audio"),
        (t.subtitles.is_some(), "--subtitles"),
        (t.mute, "--mute"),
    ]
    .into_iter()
    .filter_map(|(on, flag)| on.then_some(flag))
    .collect()
}

/// The refusal of `--compress` on a file that is not a PDF, with the two
/// steps that do it instead.
pub fn not_a_pdf(input: &Path) -> ConvError {
    refused_source(input, "--compress")
}

/// `not_a_pdf` for `how`: `--compress`, or `--max-size 5mb` on a PDF
/// target.
fn refused_source(input: &Path, how: &str) -> ConvError {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let flag = how.split(' ').next().unwrap_or(how);
    ConvError {
        remediation: Some(Remediation {
            managed: None,
            manual: Some(format!("convert it first, then `conv {stem}.pdf {how}`")),
        }),
        ..invalid(format!(
            "{flag} makes a PDF smaller, and {} is not a PDF",
            display_name(input)
        ))
    }
}

/// Reads `input` for `how` (`asked`). A PDF that needs a password to open
/// is refused with the way round, since nothing here can ask for one.
fn read_open(qpdf: &Path, input: &Path, how: &str) -> Result<read::PdfInfo> {
    read::read(qpdf, input, None).map_err(|e| {
        if e.code != ErrorCode::PasswordProtected {
            return e;
        }
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        ConvError {
            remediation: Some(Remediation {
                managed: None,
                manual: Some(format!(
                    "unlock it first, `conv {stem}.pdf --unlock`, then `conv {stem}-unlocked.pdf {how}`"
                )),
            }),
            ..e
        }
    })
}

/// Refuses what `--compress` does not do: anything but a PDF made into a
/// PDF, or with another knob.
pub(crate) fn check(from: Format, to: Format, inputs: &[PathBuf], tuning: &Tuning) -> Result<()> {
    let input = inputs.first().map(|p| display_name(p)).unwrap_or_default();
    let how = asked(tuning);
    let flag = if tuning.max_size.is_some() {
        "--max-size"
    } else {
        "--compress"
    };
    if from != Format::Pdf {
        return Err(refused_source(
            inputs.first().map_or(Path::new("file"), |p| p),
            &how,
        ));
    }
    if to != Format::Pdf {
        return Err(invalid(format!(
            "{flag} makes a PDF into a smaller PDF, not {}; drop {flag} to convert {input}",
            to.ext()
        )));
    }
    if inputs.len() != 1 {
        return Err(invalid(format!("{flag} takes one PDF at a time")));
    }
    if let Some(other) = other_knobs(tuning).first() {
        return Err(invalid(format!(
            "{flag} chooses how each image is kept, so it can't be combined with {other}"
        )));
    }
    Ok(())
}

// --- qpdf's commands --------------------------------------------------------

/// What the post-pass keeps from the input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Keep {
    /// Encrypt the output as the input is (owner restrictions stay).
    pub encryption_from: Option<PathBuf>,
    /// The input claims PDF/A-1, which forbids object streams.
    pub no_object_streams: bool,
}

/// Repairs the input, takes objects out of object streams and removes
/// owner-password protection, so lopdf can read every image.
pub(crate) fn pre_args(input: &Path, pre: &Path) -> Vec<String> {
    vec![
        path_arg(input),
        "--object-streams=disable".to_string(),
        "--decrypt".to_string(),
        path_arg(pre),
    ]
}

/// Writes the result: the scratch copy with the new images put in, object
/// streams generated, every Flate stream recompressed at level 9.
pub(crate) fn post_args(pre: &Path, update: Option<&Path>, out: &Path, keep: &Keep) -> Vec<String> {
    let mut argv = vec![path_arg(pre)];
    if let Some(u) = update {
        argv.push(format!("--update-from-json={}", u.display()));
    }
    argv.push(if keep.no_object_streams {
        "--object-streams=disable".to_string()
    } else {
        "--object-streams=generate".to_string()
    });
    argv.push("--recompress-flate".to_string());
    argv.push("--compression-level=9".to_string());
    if let Some(from) = &keep.encryption_from {
        argv.push(format!("--copy-encryption={}", from.display()));
    }
    // Never `--linearize`, even for a linearized input: on the IRS form
    // and publication it made the file 6-15% larger and qpdf twice as
    // slow, for a layout that only matters to a browser streaming a large
    // PDF page by page. The README says so.
    argv.push(path_arg(out));
    argv
}

pub(crate) fn check_args(out: &Path) -> Vec<String> {
    vec!["--check".to_string(), path_arg(out)]
}

fn catalog(doc: &Document) -> Option<&lopdf::Dictionary> {
    doc.catalog().ok()
}

/// Whether the document's XMP claims PDF/A-1.
fn claims_pdfa1(doc: &Document) -> bool {
    let Some(xmp) = catalog(doc)
        .and_then(|c| c.get(b"Metadata").ok())
        .and_then(|o| o.as_reference().ok())
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_stream().ok())
        .and_then(|s| {
            if s.dict.get(b"Filter").is_err() {
                Some(s.content.clone())
            } else {
                s.decompressed_content_with_limit(16 << 20).ok()
            }
        })
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&xmp);
    text.match_indices("pdfaid:part").any(|(i, m)| {
        text[i + m.len()..]
            .chars()
            .find(|c| c.is_ascii_digit())
            .is_some_and(|c| c == '1')
    })
}

/// What any rewrite would break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signature {
    None,
    /// Adobe Reader usage rights (`/Perms` with `/UR` or `/UR3` only): a
    /// signature Adobe adds so old versions of Reader can save a filled
    /// form. Breaking it costs that, and Reader's note that the file has
    /// changed; the document's own content is not vouched for by it.
    UsageRights,
    /// A signature field with a value, or a certification (`/DocMDP`).
    Signed,
}

/// Whether the document carries a signature a rewrite would break.
fn signature(doc: &Document) -> Signature {
    let Some(cat) = catalog(doc) else {
        return Signature::None;
    };
    let perms = cat.get(b"Perms").ok().and_then(|o| walk::dict_of(doc, o));
    if perms.is_some_and(|p| p.get(b"DocMDP").is_ok()) || signed_field(doc, cat) {
        return Signature::Signed;
    }
    if perms.is_some_and(|p| !p.is_empty()) {
        return Signature::UsageRights;
    }
    Signature::None
}

/// Whether a form field holds a signature.
fn signed_field(doc: &Document, cat: &lopdf::Dictionary) -> bool {
    let Some(fields) = cat
        .get(b"AcroForm")
        .ok()
        .and_then(|o| walk::dict_of(doc, o))
        .and_then(|f| f.get(b"Fields").ok())
        .and_then(|o| walk::resolve(doc, o))
        .and_then(|o| o.as_array().ok())
    else {
        return false;
    };
    let mut stack: Vec<(&Object, bool)> = fields.iter().map(|f| (f, false)).collect();
    let mut visited = 0;
    while let Some((f, parent_sig)) = stack.pop() {
        visited += 1;
        if visited > 100_000 {
            break;
        }
        let Some(d) = walk::dict_of(doc, f) else {
            continue;
        };
        let sig = match d.get(b"FT").ok().and_then(|o| o.as_name().ok()) {
            Some(ft) => ft == b"Sig",
            None => parent_sig,
        };
        if sig && d.get(b"V").is_ok() {
            return true;
        }
        if let Some(kids) = d
            .get(b"Kids")
            .ok()
            .and_then(|o| walk::resolve(doc, o))
            .and_then(|o| o.as_array().ok())
        {
            stack.extend(kids.iter().map(|k| (k, sig)));
        }
    }
    false
}

// --- The engine -------------------------------------------------------------

/// What one pass wrote.
#[derive(Debug, Clone)]
struct Pass {
    level: Level,
    path: PathBuf,
    bytes: u64,
    downsampled: u32,
    reencoded: u32,
}

/// What lopdf made of the scratch copy.
struct Read {
    doc: Document,
    survey: Vec<Surveyed>,
    signature: Signature,
    pdfa1: bool,
}

struct Engine<'a> {
    qpdf: &'a ResolvedBackend,
    input: PathBuf,
    /// The output's name, for messages.
    out_name: String,
    scratch: PathBuf,
    pre: PathBuf,
    read: Option<Read>,
    keep: Keep,
    on_event: &'a mut dyn FnMut(Event),
    steps: usize,
    backend_output: Vec<BackendOutput>,
    /// qpdf repaired the input.
    repaired: bool,
}

const REPORT_CAP: usize = 16 * 1024;

fn qpdf_error(message: String) -> ConvError {
    ConvError {
        backend: Some(Backend::Qpdf),
        ..ConvError::new(ErrorCode::ConversionFailed, message)
    }
}

fn io_error(what: &str, e: std::io::Error) -> ConvError {
    ConvError::new(ErrorCode::ConversionFailed, format!("{what}: {e}"))
}

impl Engine<'_> {
    /// Runs qpdf with `argv`; its exit code, 0 or 3 (warnings).
    fn qpdf(&mut self, argv: Vec<String>) -> Result<i32> {
        let index = self.steps;
        self.steps += 1;
        (self.on_event)(Event::StepStarted {
            index,
            total: 1,
            backend: Backend::Qpdf,
        });
        (self.on_event)(Event::StepSpawned {
            index,
            program: self.qpdf.path.clone(),
            argv: argv.clone(),
        });
        let out = backend_command(&self.qpdf.path)
            .args(&argv)
            .output()
            .map_err(|e| qpdf_error(format!("failed to run {}: {e}", self.qpdf.path.display())))?;
        let report = String::from_utf8_lossy(&out.stderr).into_owned();
        if !report.trim().is_empty() {
            let tail = exec::tail_str(&report, REPORT_CAP).to_string();
            (self.on_event)(Event::StepReport {
                index,
                backend: Backend::Qpdf,
                report: tail.clone(),
            });
            self.backend_output.push(BackendOutput {
                backend: Backend::Qpdf,
                stderr: tail,
            });
        }
        match out.status.code() {
            Some(c @ (0 | 3)) => {
                (self.on_event)(Event::StepFinished { index });
                Ok(c)
            }
            _ => Err(qpdf_error(format!(
                "qpdf failed: {}",
                self.hide(&qpdf_reason(&report))
            ))),
        }
    }

    /// `reason` with the scratch paths qpdf may name replaced by the names
    /// the person knows.
    fn hide(&self, reason: &str) -> String {
        let mut out = reason.replace(&self.pre.display().to_string(), &display_name(&self.input));
        for l in &LEVELS {
            let pass = self.scratch.join(format!("l{}.pdf", l.n));
            out = out.replace(&pass.display().to_string(), &self.out_name);
        }
        out.replace(
            &format!("{}{}", self.scratch.display(), std::path::MAIN_SEPARATOR),
            "",
        )
    }

    /// The pre-pass, then lopdf's read of its result.
    fn prepare(&mut self) -> Result<()> {
        let code = self.qpdf(pre_args(&self.input, &self.pre))?;
        self.repaired = code == 3;
        (self.on_event)(Event::Working {
            what: "reading images".to_string(),
        });
        self.read = std::fs::read(&self.pre).ok().and_then(|bytes| {
            let doc = Document::load_mem(&bytes).ok()?;
            let placements = walk::placements(&doc);
            let listed = walk::listed_images(&doc);
            let users = image::mask_users(&doc, &listed);
            let survey = listed
                .iter()
                .map(|&id| image::survey(&doc, id, &placements, &users))
                .collect();
            Some(Read {
                signature: signature(&doc),
                pdfa1: claims_pdfa1(&doc),
                survey,
                doc,
            })
        });
        if self.read.as_ref().is_some_and(|r| r.pdfa1) {
            self.keep.no_object_streams = true;
        }
        Ok(())
    }

    fn candidates(&self) -> Vec<Candidate> {
        self.read
            .iter()
            .flat_map(|r| r.survey.iter())
            .filter_map(|s| s.verdict.clone().ok())
            .collect()
    }

    /// Writes the new images of a pass and the update that puts them in.
    fn write_update(&self, level: &Level, done: &[Replacement]) -> Result<Option<PathBuf>> {
        let Some(read) = &self.read else {
            return Ok(None);
        };
        let trailer = update::trailer_with_size(&read.doc.trailer);
        if done.is_empty() && trailer.is_none() {
            return Ok(None);
        }
        let dir = self.scratch.join(format!("l{}", level.n));
        std::fs::create_dir_all(&dir).map_err(|e| io_error("could not write the new images", e))?;
        let mut entries = Vec::new();
        let mut add =
            |id: ObjectId, set: Vec<(&str, Object)>, data: &[u8], ext: &str| -> Result<()> {
                let Some(stream) = read
                    .doc
                    .get_object(id)
                    .ok()
                    .and_then(|o| o.as_stream().ok())
                else {
                    return Ok(());
                };
                let file = dir.join(format!("{}-{}.{ext}", id.0, id.1));
                std::fs::write(&file, data)
                    .map_err(|e| io_error("could not write the new images", e))?;
                let dict = update::new_dict(&stream.dict, &set);
                if let Some(entry) = update::stream_entry(&dict, &file) {
                    entries.push((id, entry));
                }
                Ok(())
            };
        for r in done {
            let filter = if r.jpeg { "DCTDecode" } else { "FlateDecode" };
            add(
                r.id,
                vec![
                    ("Width", Object::Integer(i64::from(r.width))),
                    ("Height", Object::Integer(i64::from(r.height))),
                    ("Filter", Object::Name(filter.as_bytes().to_vec())),
                ],
                &r.data,
                if r.jpeg { "jpg" } else { "bin" },
            )?;
            if let Some(m) = &r.mask {
                add(
                    m.id,
                    vec![
                        ("Width", Object::Integer(i64::from(m.width))),
                        ("Height", Object::Integer(i64::from(m.height))),
                        ("Filter", Object::Name(b"FlateDecode".to_vec())),
                    ],
                    &m.data,
                    "bin",
                )?;
            }
        }
        let path = self.scratch.join(format!("l{}.json", level.n));
        let json =
            serde_json::to_vec(&update::document(entries, trailer)).expect("JSON values serialise");
        std::fs::write(&path, json).map_err(|e| io_error("could not write the image update", e))?;
        Ok(Some(path))
    }

    /// One pass at `level`: the image rules, then qpdf's write.
    fn pass(&mut self, level: &Level) -> Result<Pass> {
        let done = match (&self.read, level.n) {
            (Some(read), n) if n > 0 => {
                (self.on_event)(Event::Working {
                    what: "shrinking images".to_string(),
                });
                image::shrink_all(&read.doc, &self.candidates(), level)
            }
            _ => Vec::new(),
        };
        let update = self.write_update(level, &done)?;
        let out = self.scratch.join(format!("l{}.pdf", level.n));
        self.qpdf(post_args(
            &self.pre,
            update.as_deref(),
            &out,
            &self.keep.clone(),
        ))?;
        // The new images are in the PDF now; a ladder of six passes over a
        // large scan should not hold six sets of them on disk.
        if let Some(u) = &update {
            let _ = std::fs::remove_file(u);
            let _ = std::fs::remove_dir_all(self.scratch.join(format!("l{}", level.n)));
        }
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            return Err(qpdf_error(format!(
                "qpdf wrote nothing for {}",
                display_name(&self.input)
            )));
        }
        let count = |c: Change| done.iter().filter(|r| r.change == c).count() as u32;
        Ok(Pass {
            level: *level,
            path: out,
            bytes,
            downsampled: count(Change::Downsampled),
            reencoded: count(Change::Reencoded),
        })
    }

    fn skipped(&self) -> BTreeMap<Skip, u32> {
        let mut out = BTreeMap::new();
        for s in self.read.iter().flat_map(|r| r.survey.iter()) {
            if let Err(why) = s.verdict {
                *out.entry(why).or_insert(0) += 1;
            }
        }
        out
    }

    /// Where the input's bytes are, for the "already compact" sentence.
    fn footprint(&self, file_bytes: u64) -> notes::Footprint {
        let mut f = notes::Footprint {
            file_bytes,
            ..Default::default()
        };
        let mut weighted: Vec<(f64, u64)> = Vec::new();
        for s in self.read.iter().flat_map(|r| r.survey.iter()) {
            f.image_bytes += s.bytes;
            match &s.verdict {
                Err(Skip::OneBit) => f.one_bit_bytes += s.bytes,
                Err(Skip::Jpeg2000) => f.jpx_bytes += s.bytes,
                Ok(c) => weighted.push((c.dpi, s.bytes)),
                Err(_) => {}
            }
        }
        weighted.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total: u64 = weighted.iter().map(|w| w.1).sum();
        let mut seen = 0;
        for (dpi, b) in weighted {
            seen += b;
            if seen * 2 >= total {
                // To the nearest 10 dpi: 200, not 199.7.
                f.typical_dpi = Some((dpi / 10.0).round() * 10.0);
                break;
            }
        }
        f
    }

    /// The bytes eligible images hold: an upper bound on what any level
    /// can take out.
    fn eligible_bytes(&self) -> u64 {
        self.candidates()
            .iter()
            .map(|c| c.bytes + c.mask.as_ref().map_or(0, |m| m.bytes))
            .sum()
    }
}

/// The scratch folder's location, absolute, so the data files the update
/// names resolve wherever qpdf runs.
fn scratch_in(dir: &Path) -> Result<(PathBuf, exec::ScratchGuard)> {
    let scratch = exec::make_scratch_dir(dir)?;
    let guard = exec::ScratchGuard::new(scratch.clone());
    let scratch = std::path::absolute(&scratch).unwrap_or(scratch);
    Ok((scratch, guard))
}

/// What a run settled on, before it is put in place.
struct Settled {
    /// The pass that made the file; `None` copies the input.
    pass: Option<Pass>,
    /// Passes run.
    attempts: u32,
    over_target: bool,
    suggested: Option<String>,
    /// Notes (`Outcome::warnings`) and warnings (`Outcome::notes`).
    said: Vec<String>,
    warned: Vec<String>,
}

/// `--compress` alone: L1, and L2 measured when L1 saved little.
fn compress_only(engine: &mut Engine<'_>, input_bytes: u64) -> Result<Settled> {
    let first = engine.pass(&LEVELS[COMPRESS])?;
    let mut attempts = 1;
    let smaller = first.bytes < input_bytes;
    let saved_little = first.bytes * 100 > input_bytes * (100 - SMALL_SAVING_PERCENT);
    let footprint = engine.footprint(input_bytes);
    // L2 could only reach the suggestion's share by taking it out of the
    // eligible images; when they hold less than that, it is not measured.
    let worth_measuring =
        engine.eligible_bytes() * 100 >= input_bytes * (100 - SUGGEST_AT_MOST_PERCENT);
    let suggestion = if saved_little && worth_measuring {
        let l2 = engine.pass(&LEVELS[SUGGEST])?;
        let _ = std::fs::remove_file(&l2.path);
        attempts += 1;
        (l2.bytes * 100 <= input_bytes * SUGGEST_AT_MOST_PERCENT)
            .then(|| notes::Suggestion {
                dpi: LEVELS[SUGGEST].to_dpi,
                bytes: l2.bytes,
                spelling: notes::target_above(l2.bytes),
            })
            .filter(|s| crate::size::parse(&s.spelling).is_ok_and(|t| t.bytes < input_bytes))
    } else {
        None
    };
    let why = notes::why_compact(&footprint);
    let mut said = Vec::new();
    if !smaller {
        said.push(notes::already_compact(&why, suggestion.as_ref()));
        return Ok(Settled {
            pass: None,
            attempts,
            over_target: false,
            suggested: suggestion.map(|s| s.spelling),
            said,
            warned: Vec::new(),
        });
    }
    let summary = notes::summary(first.downsampled, first.reencoded, first.level.to_dpi);
    let percent = (input_bytes - first.bytes) * 100 / input_bytes.max(1);
    match (saved_little, &suggestion, summary) {
        (true, Some(s), summary) => {
            said.extend(summary);
            said.push(notes::small_saving(percent, &why, Some(s)));
        }
        (true, None, summary) => {
            said.extend(summary);
            said.push(notes::small_saving(percent, &why, None));
        }
        (false, _, Some(summary)) => said.push(summary),
        (false, _, None) => said.push(notes::lossless_only()),
    }
    Ok(Settled {
        pass: Some(first),
        attempts,
        over_target: false,
        suggested: suggestion.map(|s| s.spelling),
        said,
        warned: Vec::new(),
    })
}

/// `--max-size`: each level in turn, from L0 (L1 with `--compress`), until
/// one comes out under the target. Measured, never predicted. L4 and L5
/// need consent; without it the run stops before them with the size L3
/// reached as the target to try.
fn ladder(
    engine: &mut Engine<'_>,
    input_bytes: u64,
    max: &crate::size::MaxSize,
    from_compress: bool,
    allow_extreme: bool,
) -> Result<Settled> {
    let name = display_name(&engine.input);
    let target = max.label();
    let start = usize::from(from_compress);
    // Without an image to change, every level is the lossless rewrite.
    let top = if engine.candidates().is_empty() {
        start
    } else {
        LEVELS.len() - 1
    };
    let mut attempts = 0;
    let mut best: Option<Pass> = None;
    let mut readable: Option<u64> = None;
    for level in &LEVELS[start..=top] {
        if usize::from(level.n) >= EXTREME && !allow_extreme {
            let suggested = readable
                .filter(|&b| b < input_bytes)
                .map(notes::target_above);
            return Err(ConvError {
                remediation: Some(Remediation {
                    managed: None,
                    manual: Some(notes::extreme_fix(suggested.as_deref())),
                }),
                ..ConvError::new(
                    ErrorCode::ConfirmationRequired,
                    notes::extreme(&name, &target, LEVELS[EXTREME].to_dpi),
                )
            });
        }
        let pass = engine.pass(level)?;
        attempts += 1;
        // The smallest file at a readable level, for the size to try.
        if usize::from(level.n) < EXTREME {
            readable = Some(readable.map_or(pass.bytes, |r: u64| r.min(pass.bytes)));
        }
        let fits = pass.bytes <= max.bytes;
        // Only the smallest pass so far is kept on disk.
        match &best {
            Some(b) if pass.bytes >= b.bytes => {
                let _ = std::fs::remove_file(&pass.path);
            }
            _ => {
                if let Some(old) = best.replace(pass) {
                    let _ = std::fs::remove_file(&old.path);
                }
            }
        }
        if fits {
            break;
        }
    }
    let best = best.expect("at least one level runs");
    let over_target = best.bytes > max.bytes;
    let mut said = vec![notes::sized(
        &best.level,
        best.downsampled,
        best.reencoded,
        attempts,
    )];
    let mut warned = Vec::new();
    if usize::from(best.level.n) >= EXTREME {
        warned.push(notes::extreme_done(&name, best.level.to_dpi));
    }
    let footprint = engine.footprint(input_bytes);
    if over_target {
        said.clear();
        warned.push(notes::over_target(
            &target,
            best.bytes.min(input_bytes),
            notes::why_over(&footprint).as_deref(),
        ));
    }
    // Never larger than the input, even over target.
    let pass = (best.bytes < input_bytes).then_some(best);
    Ok(Settled {
        pass,
        attempts,
        over_target,
        suggested: if over_target {
            readable
                .filter(|&b| b < input_bytes)
                .map(notes::target_above)
        } else {
            None
        },
        said,
        warned,
    })
}

/// Runs `--compress` or `--max-size` on a PDF for `exec::run`, which has
/// already checked the input exists, the output name, and that the output
/// may be written.
pub(crate) fn run(
    req: &Request,
    resolver: &Resolver,
    on_event: &mut dyn FnMut(Event),
) -> Result<Outcome> {
    check(req.from, req.to, &req.inputs, &req.tuning)?;
    let qpdf = resolver.resolve(Backend::Qpdf)?;
    let input = req.inputs[0].clone();
    let name = display_name(&input);
    let out_name = display_name(&req.output);
    let info = read_open(&qpdf.path, &input, &asked(&req.tuning))?;
    let input_bytes = std::fs::metadata(&input)
        .map_err(|e| io_error(&format!("could not read {name}"), e))?
        .len();
    let max = req.tuning.max_size.as_ref();
    let family = max.map_or(UnitFamily::Decimal, |m| m.family);
    let report = |strategy, attempts, over_target, suggested, pdf| SizingReport {
        kind: SizedKind::Pdf,
        target_bytes: max.map(|m| m.bytes),
        family,
        strategy,
        width: None,
        height: None,
        fps: None,
        video_bps: None,
        audio_bps: Vec::new(),
        attempts,
        cost: None,
        over_target,
        suggested,
        pdf: Some(pdf),
    };
    let outcome = |bytes, said, warned, backend_output, sizing| Outcome {
        output: req.output.clone(),
        bytes,
        warnings: said,
        notes: warned,
        backend_output,
        backends: vec![(Backend::Qpdf, qpdf.version.clone())],
        remuxed: false,
        elapsed_ms: 0,
        sizing: Some(sizing),
        enlarged: None,
        range: None,
        speed: None,
        rotation: None,
        pages: None,
        crop: None,
        audio: None,
        subtitles: None,
    };

    // A file already under its target is copied as it is, as video is.
    if let Some(m) = max.filter(|m| input_bytes <= m.bytes) {
        let temp = req
            .output
            .with_file_name(format!(".{out_name}.convkit-copy"));
        exec::copy_fresh(&input, &temp)?;
        std::fs::rename(&temp, &req.output)
            .map_err(|e| io_error(&format!("could not write {}", req.output.display()), e))?;
        let pdf = PdfSizing {
            level: None,
            source_bytes: input_bytes,
            images_downsampled: 0,
            images_reencoded: 0,
            images_skipped: BTreeMap::new(),
        };
        return Ok(outcome(
            input_bytes,
            vec![notes::already_under(input_bytes, m)],
            Vec::new(),
            Vec::new(),
            report(Strategy::Copy, 0, false, None, pdf),
        ));
    }

    let dest_dir = req
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (scratch, _guard) = scratch_in(dest_dir)?;
    let keep = Keep {
        encryption_from: info
            .encrypted
            .then(|| std::path::absolute(&input).unwrap_or_else(|_| input.clone())),
        no_object_streams: false,
    };
    let mut engine = Engine {
        qpdf: &qpdf,
        input: input.clone(),
        out_name: out_name.clone(),
        pre: scratch.join("pre.pdf"),
        scratch: scratch.clone(),
        read: None,
        keep,
        on_event,
        steps: 0,
        backend_output: Vec::new(),
        repaired: false,
    };
    engine.prepare()?;
    let signature = engine
        .read
        .as_ref()
        .map_or(Signature::None, |r| r.signature);
    if signature == Signature::Signed && !req.allow_extreme {
        return Err(ConvError {
            remediation: Some(Remediation {
                managed: None,
                manual: Some(notes::signed_fix()),
            }),
            ..ConvError::new(ErrorCode::ConfirmationRequired, notes::signed(&name))
        });
    }

    let mut settled = match max {
        None => compress_only(&mut engine, input_bytes)?,
        Some(m) => ladder(
            &mut engine,
            input_bytes,
            m,
            req.tuning.compress,
            req.allow_extreme,
        )?,
    };
    let temp_final = scratch.join("final.pdf");
    let (strategy, bytes, level, downsampled, reencoded) = match &settled.pass {
        Some(p) => {
            engine.qpdf(check_args(&p.path))?;
            std::fs::rename(&p.path, &temp_final)
                .map_err(|e| io_error("could not keep the result", e))?;
            (
                Strategy::Encode,
                p.bytes,
                Some(p.level.n),
                p.downsampled,
                p.reencoded,
            )
        }
        None => {
            exec::copy_fresh(&input, &temp_final)?;
            (Strategy::Copy, input_bytes, None, 0, 0)
        }
    };
    if engine.read.is_none() {
        settled.said.push(notes::unreadable(&name));
    }
    let mut warned = Vec::new();
    if engine.repaired || info.damaged {
        warned.push(notes::repaired(&name, &out_name));
    }
    match (signature, strategy) {
        (Signature::Signed, Strategy::Encode) => {
            warned.push(notes::signature_dropped(&name, &out_name));
        }
        (Signature::UsageRights, Strategy::Encode) => {
            warned.push(notes::usage_rights_dropped(&name, &out_name));
        }
        _ => {}
    }
    warned.append(&mut settled.warned);

    std::fs::rename(&temp_final, &req.output)
        .map_err(|e| io_error(&format!("could not write {}", req.output.display()), e))?;

    let pdf = PdfSizing {
        level,
        source_bytes: input_bytes,
        images_downsampled: downsampled,
        images_reencoded: reencoded,
        images_skipped: engine.skipped(),
    };
    let backend_output = std::mem::take(&mut engine.backend_output);
    Ok(outcome(
        bytes,
        settled.said,
        warned,
        backend_output,
        report(
            strategy,
            settled.attempts,
            settled.over_target,
            settled.suggested,
            pdf,
        ),
    ))
}

// --- Plans --------------------------------------------------------------------

/// `out.pdf` -> `out.convkit-step0.pdf`, the name a dry run gives the
/// scratch copy, as other multi-step plans name their intermediates.
fn beside(output: &Path, tail: &str) -> PathBuf {
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!("{stem}.convkit-{tail}"))
}

fn step(argv: Vec<String>, output: PathBuf, intermediate: bool) -> PlannedStep {
    PlannedStep {
        backend: Backend::Qpdf,
        program: "qpdf".to_string(),
        argv,
        output_mode: OutputMode::Path,
        output,
        intermediate_ext: intermediate.then(|| "pdf".to_string()),
        path_args: Vec::new(),
        prepare: Vec::new(),
        stdin: None,
    }
}

fn plan_with(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    keep: &Keep,
    images: bool,
    warnings: Vec<String>,
) -> ConversionPlan {
    let pre = beside(output, "step0.pdf");
    let update = images.then(|| beside(output, "images.json"));
    ConversionPlan {
        from,
        to,
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        steps: vec![
            step(pre_args(&inputs[0], &pre), pre.clone(), true),
            step(
                post_args(&pre, update.as_deref(), output, keep),
                output.to_path_buf(),
                false,
            ),
            step(check_args(output), output.to_path_buf(), false),
        ],
        warnings,
        sizing: None,
        enlarged: None,
        range: None,
        speed: None,
        rotation: None,
        built_in: false,
        pages: None,
        crop: None,
        audio: None,
        subtitles: None,
    }
}

/// The plan `plan::build_tuned` gives without reading the file: qpdf's
/// commands as they run on an unencrypted PDF, and what the image pass
/// does. `preview` reads the file for the real numbers.
pub(crate) fn plan(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    tuning: &Tuning,
) -> Result<ConversionPlan> {
    check(from, to, inputs, tuning)?;
    let l = &LEVELS[COMPRESS];
    let note = match &tuning.max_size {
        Some(m) => notes::ladder(&LEVELS, usize::from(tuning.compress), EXTREME, &m.label()),
        None => format!(
            "Between the two qpdf passes conv downsamples images drawn above {} dpi to {} dpi \
             and re-encodes high-quality JPEGs and large lossless photos as JPEG at quality {}; \
             the original is copied if the result is not smaller.",
            l.above_dpi, l.to_dpi, l.quality
        ),
    };
    Ok(plan_with(
        from,
        to,
        inputs,
        output,
        &Keep::default(),
        true,
        vec![note],
    ))
}

/// `--dry-run`: the commands a real run makes, and what it would do to
/// the images, from a pre-pass into the system's temporary folder. Writes
/// nothing beside the input or the output.
pub fn preview(
    from: Format,
    to: Format,
    inputs: &[PathBuf],
    output: &Path,
    tuning: &Tuning,
    resolver: &Resolver,
) -> Result<ConversionPlan> {
    check(from, to, inputs, tuning)?;
    let input = &inputs[0];
    if !input.is_file() {
        return Err(ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", input.display()),
        ));
    }
    let qpdf = resolver.resolve(Backend::Qpdf)?;
    let info = read_open(&qpdf.path, input, &asked(tuning))?;
    let input_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    if let Some(m) = tuning.max_size.as_ref().filter(|m| input_bytes <= m.bytes) {
        let mut p = plan_with(
            from,
            to,
            inputs,
            output,
            &Keep::default(),
            false,
            Vec::new(),
        );
        p.steps.clear();
        p.warnings
            .push(notes::already_under(input_bytes, m).replace("; copied", "; it would be copied"));
        return Ok(p);
    }
    let (scratch, _guard) = scratch_in(&std::env::temp_dir())?;
    let mut ignore = |_e: Event| {};
    let mut engine = Engine {
        qpdf: &qpdf,
        input: input.clone(),
        out_name: display_name(output),
        pre: scratch.join("pre.pdf"),
        scratch: scratch.clone(),
        read: None,
        keep: Keep {
            encryption_from: info.encrypted.then(|| input.clone()),
            no_object_streams: false,
        },
        on_event: &mut ignore,
        steps: 0,
        backend_output: Vec::new(),
        repaired: false,
    };
    engine.prepare()?;
    let l = &LEVELS[COMPRESS];
    let mut warnings = Vec::new();
    if let Some(m) = &tuning.max_size {
        warnings.push(notes::ladder(
            &LEVELS,
            usize::from(tuning.compress),
            EXTREME,
            &m.label(),
        ));
    }
    let name = display_name(input);
    let candidates = engine.candidates();
    match &engine.read {
        Some(read) => {
            let image_bytes: u64 = read.survey.iter().map(|s| s.bytes).sum();
            let above = candidates
                .iter()
                .filter(|c| image::target_size(c, l).is_some())
                .count();
            warnings.push(notes::dry_run(
                read.survey.len() as u32,
                image_bytes,
                input_bytes,
                above as u32,
                tuning.max_size.is_none().then_some(l),
            ));
            let skipped: Vec<(Skip, u32)> = engine.skipped().into_iter().collect();
            warnings.extend(notes::skipped_note(&skipped));
            match read.signature {
                Signature::Signed => {
                    warnings.push(format!("{}; a real run asks first.", notes::signed(&name)));
                }
                Signature::UsageRights => {
                    warnings.push(notes::usage_rights_dropped(&name, &display_name(output)));
                }
                Signature::None => {}
            }
        }
        None => warnings.push(notes::unreadable(&name)),
    }
    if engine.repaired || info.damaged {
        warnings.push(notes::repaired(&name, &display_name(output)));
    }
    let keep = engine.keep.clone();
    Ok(plan_with(
        from,
        to,
        inputs,
        output,
        &keep,
        !candidates.is_empty(),
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_starts_lossless_and_gets_harder() {
        assert_eq!(LEVELS[0].n, 0);
        for pair in LEVELS.windows(2) {
            assert_eq!(pair[1].n, pair[0].n + 1);
            assert!(pair[1].to_dpi <= pair[0].to_dpi);
            assert!(pair[1].quality <= pair[0].quality);
            assert!(pair[1].above_dpi >= pair[1].to_dpi);
        }
    }

    #[test]
    fn only_a_pdf_into_a_pdf_with_no_other_knob() {
        let one = vec![PathBuf::from("report.pdf")];
        let t = Tuning {
            compress: true,
            ..Tuning::default()
        };
        check(Format::Pdf, Format::Pdf, &one, &t).unwrap();

        let docx = vec![PathBuf::from("dir/report.docx")];
        let e = check(Format::Docx, Format::Pdf, &docx, &t).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "--compress makes a PDF smaller, and report.docx is not a PDF"
        );
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "convert it first, then `conv report.pdf --compress`"
        );

        let e = check(Format::Pdf, Format::Jpg, &one, &t).unwrap_err();
        assert_eq!(
            e.message,
            "--compress makes a PDF into a smaller PDF, not jpg; drop --compress to convert report.pdf"
        );
        let sized = Tuning {
            max_size: Some(crate::size::parse("5mb").unwrap()),
            ..Tuning::default()
        };
        check(Format::Pdf, Format::Pdf, &one, &sized).unwrap();
        let both = Tuning {
            compress: true,
            ..sized.clone()
        };
        check(Format::Pdf, Format::Pdf, &one, &both).unwrap();
        let e = check(Format::Docx, Format::Pdf, &docx, &sized).unwrap_err();
        assert_eq!(
            e.message,
            "--max-size makes a PDF smaller, and report.docx is not a PDF"
        );
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "convert it first, then `conv report.pdf --max-size 5mb`"
        );
        assert!(applies(&sized, Format::Docx, Format::Pdf));
        assert!(!applies(&sized, Format::Mov, Format::Mp4));
        assert!(
            applies(&t, Format::Mov, Format::Mp4),
            "refused by check, by name"
        );

        let q = Tuning {
            quality: Some(50),
            ..t.clone()
        };
        let e = check(Format::Pdf, Format::Pdf, &one, &q).unwrap_err();
        assert_eq!(
            e.message,
            "--compress chooses how each image is kept, so it can't be combined with --quality"
        );
    }

    #[test]
    fn qpdfs_commands() {
        let pre = Path::new("s/pre.pdf");
        assert_eq!(
            pre_args(Path::new("-scan.pdf"), pre),
            vec![
                "./-scan.pdf",
                "--object-streams=disable",
                "--decrypt",
                "s/pre.pdf"
            ]
        );
        assert_eq!(
            post_args(
                pre,
                Some(Path::new("s/l1.json")),
                Path::new("out.pdf"),
                &Keep::default()
            ),
            vec![
                "s/pre.pdf",
                "--update-from-json=s/l1.json",
                "--object-streams=generate",
                "--recompress-flate",
                "--compression-level=9",
                "out.pdf"
            ]
        );
        let keep = Keep {
            encryption_from: Some(PathBuf::from("in.pdf")),
            no_object_streams: true,
        };
        assert_eq!(
            post_args(pre, None, Path::new("out.pdf"), &keep),
            vec![
                "s/pre.pdf",
                "--object-streams=disable",
                "--recompress-flate",
                "--compression-level=9",
                "--copy-encryption=in.pdf",
                "out.pdf"
            ]
        );
        assert_eq!(check_args(Path::new("out.pdf")), vec!["--check", "out.pdf"]);
    }

    #[test]
    fn the_plan_lists_three_qpdf_calls_without_reading_the_file() {
        let t = Tuning {
            compress: true,
            ..Tuning::default()
        };
        let p = plan(
            Format::Pdf,
            Format::Pdf,
            &[PathBuf::from("scan.pdf")],
            Path::new("scan-compressed.pdf"),
            &t,
        )
        .unwrap();
        assert_eq!(p.steps.len(), 3);
        assert_eq!(
            p.steps[0].argv.last().unwrap(),
            "scan-compressed.convkit-step0.pdf"
        );
        assert!(p.steps[1]
            .argv
            .contains(&"--update-from-json=scan-compressed.convkit-images.json".to_string()));
        assert_eq!(p.steps[2].argv, vec!["--check", "scan-compressed.pdf"]);
        assert!(p.warnings[0].contains("above 225 dpi to 150 dpi"));
    }

    fn doc_with_catalog(entries: Vec<(&str, Object)>) -> Document {
        let mut doc = Document::with_version("1.7");
        let mut cat = lopdf::dictionary! { "Type" => "Catalog" };
        for (k, v) in entries {
            cat.set(k, v);
        }
        let id = doc.add_object(cat);
        doc.trailer.set("Root", id);
        doc
    }

    fn signed(doc: &Document) -> bool {
        signature(doc) == Signature::Signed
    }

    #[test]
    fn a_signature_with_a_value_or_a_certification_counts_as_signed() {
        use lopdf::dictionary;
        assert_eq!(signature(&doc_with_catalog(vec![])), Signature::None);
        let certified = dictionary! { "DocMDP" => dictionary! {} };
        assert!(signed(&doc_with_catalog(vec![("Perms", certified.into())])));
        let rights = dictionary! { "UR3" => dictionary! {} };
        assert_eq!(
            signature(&doc_with_catalog(vec![("Perms", rights.into())])),
            Signature::UsageRights
        );

        let mut doc = doc_with_catalog(vec![]);
        let empty_sig =
            doc.add_object(dictionary! { "FT" => "Sig", "T" => Object::string_literal("s") });
        let form = doc.add_object(dictionary! { "Fields" => vec![empty_sig.into()] });
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_object_mut(root)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("AcroForm", form);
        assert!(!signed(&doc), "an unsigned signature field");

        let value = doc.add_object(dictionary! { "Type" => "Sig" });
        let kid = doc.add_object(dictionary! { "V" => value });
        let parent = doc.add_object(dictionary! { "FT" => "Sig", "Kids" => vec![kid.into()] });
        doc.get_object_mut(form)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Fields", vec![empty_sig.into(), parent.into()]);
        assert!(signed(&doc), "a kid inherits /FT from its parent");
    }

    #[test]
    fn pdfa_1_is_read_from_the_xmp() {
        use lopdf::{dictionary, Stream};
        let mut doc = doc_with_catalog(vec![]);
        let xmp = |part: &str| {
            format!("<x:xmpmeta><rdf:Description pdfaid:part=\"{part}\" pdfaid:conformance=\"B\"/></x:xmpmeta>")
        };
        let m = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata" },
            xmp("1").into_bytes(),
        ));
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_object_mut(root)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Metadata", m);
        assert!(claims_pdfa1(&doc));
        doc.objects.insert(
            m,
            Object::Stream(Stream::new(
                dictionary! {},
                b"<pdfaid:part>2</pdfaid:part>".to_vec(),
            )),
        );
        assert!(!claims_pdfa1(&doc));
    }
}
