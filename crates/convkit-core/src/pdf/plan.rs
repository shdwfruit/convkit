//! Turning inputs and ranges into qpdf calls, the files they write, and
//! what conv should tell the person about the result.

use std::path::{Path, PathBuf};

use super::lock::{Lock, Password};
use super::range::{coverage, join_and, ordered_runs, spans, PageRange, Resolved};
use super::read::{self, password_arg, AskPassword, PdfInfo};
use super::{display_name, path_arg};
use crate::error::{ConvError, ErrorCode, Result};

/// What a plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfJob {
    /// The first input is qpdf's primary input, so its bookmarks, metadata
    /// and form survive; see spec §4.1. `passwords` holds each input's,
    /// in the same order.
    Merge {
        inputs: Vec<PathBuf>,
        passwords: Vec<Option<Password>>,
        output: PathBuf,
    },
    /// One qpdf `--split-pages` pass, numbering files as qpdf does.
    SplitPages {
        input: PathBuf,
        password: Option<Password>,
        dir: PathBuf,
        stem: String,
    },
    /// One qpdf call per range, each from an empty document so no bookmark
    /// points at a page the output lacks.
    SplitRanges {
        input: PathBuf,
        password: Option<Password>,
        ranges: Vec<Resolved>,
    },
    /// `conv pages`: the input as qpdf's primary input, so its bookmarks,
    /// metadata and forms stay, with `pages` (source pages in output order;
    /// `None` keeps every page as it is) and `turns` (clockwise degrees,
    /// and the output pages turned by that much).
    Pages {
        input: PathBuf,
        password: Option<Password>,
        output: PathBuf,
        pages: Option<Vec<u32>>,
        turns: Vec<(u16, Vec<u32>)>,
    },
    /// The input rewritten as it is: unlocked, or locked with the plan's
    /// `lock`.
    Rewrite {
        input: PathBuf,
        password: Option<Password>,
        output: PathBuf,
    },
}

/// What `plan_rewrite` does to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewrite {
    Lock,
    Unlock,
}

/// One file a plan writes, and the source pages in it (for a merge, the
/// output's own page numbers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedOutput {
    pub path: PathBuf,
    pub pages: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfPlan {
    pub job: PdfJob,
    pub outputs: Vec<PlannedOutput>,
    /// Printed as `note` lines; JSON `warnings`.
    pub notes: Vec<String>,
    /// Printed as `warning` lines on stderr; JSON `notes`.
    pub warnings: Vec<String>,
    /// Some input was damaged and qpdf repaired it while reading.
    pub repaired: bool,
    /// Locks every output; see `set_lock`.
    pub lock: Option<Lock>,
    /// The note that a password-protected input's output is not, which a
    /// lock takes back.
    unlocked_note: Option<String>,
    /// The note that an input's restrictions are dropped, which a lock with
    /// restrictions of its own takes back: its note says what is off.
    restricted_note: Option<String>,
    /// What the outputs are called in a note: `out.pdf`, `The split files`.
    what: String,
}

/// Where a merge, a page edit or a rewrite writes inside the scratch
/// folder.
const SINGLE_SCRATCH: &str = "out.pdf";

/// The `--split-pages` template of a real run, relative to the scratch
/// folder.
pub(crate) const SPLIT_SCRATCH_TEMPLATE: &str = "p-%d.pdf";

/// Every argument the input `p` needs: its path, then its password.
fn opened(p: &Path, password: Option<&Password>, reveal: bool) -> Vec<String> {
    let mut v = vec![path_arg(p)];
    v.extend(password.map(|pw| password_arg(pw, reveal)));
    v
}

impl PdfPlan {
    pub(crate) fn new(job: PdfJob, outputs: Vec<PlannedOutput>, what: String) -> PdfPlan {
        PdfPlan {
            job,
            outputs,
            notes: Vec::new(),
            warnings: Vec::new(),
            repaired: false,
            lock: None,
            unlocked_note: None,
            restricted_note: None,
            what,
        }
    }

    pub fn is_merge(&self) -> bool {
        matches!(self.job, PdfJob::Merge { .. })
    }

    /// Writes one file: anything but a split.
    pub fn writes_one_file(&self) -> bool {
        !matches!(
            self.job,
            PdfJob::SplitPages { .. } | PdfJob::SplitRanges { .. }
        )
    }

    /// Locks every output with `lock`. The note that a locked input's
    /// output is not locked goes, and a restricted lock says what it turns
    /// off.
    ///
    /// A password beyond plain ASCII gets a warning: qpdf writes it as the
    /// PDF standard says (UTF-8), and poppler, Ghostscript and pypdf open
    /// the file with it, but Apple's PDF engine, so Preview and Safari,
    /// accepts no such password at all.
    pub fn set_lock(&mut self, lock: Lock) {
        let restricts = !lock.restrict.is_empty();
        let mut gone: Vec<String> = self.unlocked_note.take().into_iter().collect();
        if restricts {
            gone.extend(self.restricted_note.take());
        }
        self.notes.retain(|n| !gone.contains(n));
        if let Some(note) = lock.note(&self.what) {
            self.notes.push(note);
        }
        if !lock.password.expose().is_ascii() {
            self.warnings.push(
                "Apple's Preview can't open a PDF whose password has characters beyond plain \
                 ASCII, such as é or ü; most other PDF apps can."
                    .to_string(),
            );
        }
        self.lock = Some(lock);
    }

    /// Whether some call carries a password, so a real run must hand qpdf
    /// its arguments where no other user can read them.
    pub(crate) fn has_secrets(&self) -> bool {
        self.lock.is_some()
            || match &self.job {
                PdfJob::Merge { passwords, .. } => passwords.iter().any(Option::is_some),
                PdfJob::SplitPages { password, .. }
                | PdfJob::SplitRanges { password, .. }
                | PdfJob::Pages { password, .. }
                | PdfJob::Rewrite { password, .. } => password.is_some(),
            }
    }

    /// Every argument that carries a password, as qpdf would echo it back in
    /// an error, with what to show instead.
    pub(crate) fn secret_args(&self) -> Vec<(String, String)> {
        let mut v = Vec::new();
        let mut add = |p: &Password| {
            v.push((password_arg(p, true), password_arg(p, false)));
        };
        match &self.job {
            PdfJob::Merge { passwords, .. } => passwords.iter().flatten().for_each(&mut add),
            PdfJob::SplitPages { password, .. }
            | PdfJob::SplitRanges { password, .. }
            | PdfJob::Pages { password, .. }
            | PdfJob::Rewrite { password, .. } => password.iter().for_each(&mut add),
        }
        if let Some(lock) = &self.lock {
            v.extend(
                lock.args(true)
                    .into_iter()
                    .zip(lock.args(false))
                    .filter(|(a, b)| a != b),
            );
        }
        v
    }

    /// qpdf's arguments for each call, writing the final outputs: what
    /// `--dry-run` prints, with every password shown as a placeholder.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands_into(None, false)
    }

    /// The same calls writing into `scratch` instead, as `run` makes them,
    /// with the passwords themselves when `reveal` is set.
    pub(crate) fn commands_into(&self, scratch: Option<&Path>, reveal: bool) -> Vec<Vec<String>> {
        let single =
            |output: &PathBuf| scratch.map_or_else(|| output.clone(), |s| s.join(SINGLE_SCRATCH));
        // `--decrypt` drops the input's own lock; a lock of conv's replaces
        // it anyway, so it is left out there.
        let decrypt = || self.lock.is_none().then(|| "--decrypt".to_string());
        let lock = || {
            self.lock
                .as_ref()
                .map(|l| l.args(reveal))
                .unwrap_or_default()
        };
        match &self.job {
            PdfJob::Merge {
                inputs,
                passwords,
                output,
            } => {
                let mut argv: Vec<String> = decrypt().into_iter().collect();
                if let Some(p) = &passwords[0] {
                    argv.push(password_arg(p, reveal));
                }
                argv.push(path_arg(&inputs[0]));
                if inputs.len() > 1 {
                    argv.push("--pages".to_string());
                    argv.push(".".to_string());
                    for (input, password) in inputs[1..].iter().zip(&passwords[1..]) {
                        argv.extend(opened(input, password.as_ref(), reveal));
                    }
                    argv.push("--".to_string());
                }
                argv.extend(lock());
                argv.push(path_arg(&single(output)));
                vec![argv]
            }
            PdfJob::SplitPages {
                input,
                password,
                dir,
                stem,
            } => {
                // qpdf substitutes the first `%d` anywhere in the template,
                // so a real run uses the bare template from inside the
                // scratch folder (see `runs_in_scratch`), where no `%` in
                // the person's own paths can reach it.
                let (input, template) = match scratch {
                    Some(_) => (
                        std::path::absolute(input).unwrap_or_else(|_| input.clone()),
                        PathBuf::from(SPLIT_SCRATCH_TEMPLATE),
                    ),
                    None => (input.clone(), dir.join(format!("{stem}-%d.pdf"))),
                };
                let mut argv: Vec<String> = decrypt().into_iter().collect();
                argv.extend(password.as_ref().map(|p| password_arg(p, reveal)));
                argv.push("--split-pages".to_string());
                argv.push(path_arg(&input));
                argv.extend(lock());
                argv.push(path_arg(&template));
                vec![argv]
            }
            PdfJob::SplitRanges {
                input,
                password,
                ranges,
            } => ranges
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let out = match scratch {
                        Some(s) => s.join(format!("r-{}.pdf", i + 1)),
                        None => self.outputs[i].path.clone(),
                    };
                    let mut argv: Vec<String> = decrypt().into_iter().collect();
                    argv.push("--empty".to_string());
                    argv.push("--pages".to_string());
                    argv.extend(opened(input, password.as_ref(), reveal));
                    argv.push(r.label());
                    argv.push("--".to_string());
                    argv.extend(lock());
                    argv.push(path_arg(&out));
                    argv
                })
                .collect(),
            PdfJob::Pages {
                input,
                password,
                output,
                pages,
                turns,
            } => {
                let mut argv: Vec<String> = decrypt().into_iter().collect();
                argv.extend(password.as_ref().map(|p| password_arg(p, reveal)));
                argv.push(path_arg(input));
                if let Some(pages) = pages {
                    argv.push("--pages".to_string());
                    argv.push(".".to_string());
                    argv.push(ordered_runs(pages).join(","));
                    argv.push("--".to_string());
                }
                // qpdf numbers these by the output's pages. `+` turns from
                // where each page already is, as `--rotate` does a photo.
                for (degrees, on) in turns {
                    argv.push(format!(
                        "--rotate=+{degrees}:{}",
                        ordered_runs(on).join(",")
                    ));
                }
                argv.extend(lock());
                argv.push(path_arg(&single(output)));
                vec![argv]
            }
            PdfJob::Rewrite {
                input,
                password,
                output,
            } => {
                let mut argv: Vec<String> = decrypt().into_iter().collect();
                argv.extend(password.as_ref().map(|p| password_arg(p, reveal)));
                argv.push(path_arg(input));
                argv.extend(lock());
                argv.push(path_arg(&single(output)));
                vec![argv]
            }
        }
    }

    /// Whether a real run must start qpdf with the scratch folder as its
    /// working directory, because its output names are relative to it.
    pub(crate) fn runs_in_scratch(&self) -> bool {
        matches!(self.job, PdfJob::SplitPages { .. })
    }

    /// The name the template's output files get once placed, e.g.
    /// `report-%d.pdf`, for `SplitPages` plans.
    pub(crate) fn final_template_name(&self) -> Option<String> {
        match &self.job {
            PdfJob::SplitPages { stem, .. } => Some(format!("{stem}-%d.pdf")),
            _ => None,
        }
    }

    /// Where output `i` lands inside `scratch` when the commands write
    /// there. For `SplitPages` this follows qpdf's own `%d` padding: the
    /// width of the page count.
    pub(crate) fn scratch_output(&self, scratch: &Path, i: usize) -> PathBuf {
        match &self.job {
            PdfJob::Merge { .. } | PdfJob::Pages { .. } | PdfJob::Rewrite { .. } => {
                scratch.join(SINGLE_SCRATCH)
            }
            PdfJob::SplitPages { .. } => {
                let width = self.outputs.len().to_string().len();
                scratch.join(format!("p-{:0width$}.pdf", i + 1))
            }
            PdfJob::SplitRanges { .. } => scratch.join(format!("r-{}.pdf", i + 1)),
        }
    }
}

/// Unique file names of `infos` matching `pick`, in order.
fn names_where(infos: &[PdfInfo], pick: impl Fn(&PdfInfo) -> bool) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for i in infos.iter().filter(|i| pick(i)) {
        let n = display_name(&i.path);
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

pub(crate) fn restricted_note(names: &[String], result: &str) -> String {
    let verb = if names.len() == 1 {
        "restricts"
    } else {
        "restrict"
    };
    format!(
        "{} {verb} printing, copying or editing; {result}.",
        join_and(names)
    )
}

/// `a.pdf was password-protected; out.pdf isn't. Add --password to lock
/// it.` `isnt` is the result's half: `out.pdf isn't`, `the split files
/// aren't`.
fn unlocked_note(names: &[String], isnt: &str, it: &str) -> String {
    let verb = if names.len() == 1 { "was" } else { "were" };
    format!(
        "{} {verb} password-protected; {isnt}. Add --password to lock {it}.",
        join_and(names)
    )
}

pub(crate) fn repaired_warning(names: &[String], check: &str) -> String {
    if names.len() == 1 {
        format!(
            "{} was damaged and has been repaired; check {check}.",
            names[0]
        )
    } else {
        format!(
            "{} were damaged and have been repaired; check {check}.",
            join_and(names)
        )
    }
}

fn left_out_warning(pages: &[u32]) -> String {
    if let [one] = pages {
        format!("Page {one} is not in any range, so it was left out.")
    } else {
        format!(
            "Pages {} are not in any range, so they were left out.",
            spans(pages)
        )
    }
}

fn overlap_note(pages: &[u32]) -> String {
    if let [one] = pages {
        format!("Page {one} is in more than one range, so it appears in more than one file.")
    } else {
        format!(
            "Pages {} are in more than one range, so they appear in more than one file.",
            spans(pages)
        )
    }
}

/// The notes and warnings every plan of one input shares: dropped
/// restrictions, a password the output lacks, and a repair. `subject`
/// names the result in a sentence: `out.pdf`, or `the split files`
/// (`plural`).
pub(crate) fn source_notes(plan: &mut PdfPlan, info: &PdfInfo, subject: &str, plural: bool) {
    let name = vec![display_name(&info.path)];
    let (does, isnt, it) = if plural {
        ("do", "aren't", "them")
    } else {
        ("does", "isn't", "it")
    };
    if info.restricted {
        let note = restricted_note(&name, &format!("{subject} {does} not"));
        plan.notes.push(note.clone());
        plan.restricted_note = Some(note);
    }
    if info.locked() {
        let note = unlocked_note(&name, &format!("{subject} {isnt}"), it);
        plan.notes.push(note.clone());
        plan.unlocked_note = Some(note);
    }
    if info.damaged {
        plan.warnings.push(repaired_warning(&name, subject));
        plan.repaired = true;
    }
}

/// Plans a merge of `infos` (one per input, in order, repeats included).
pub fn merge_plan(infos: &[PdfInfo], output: &Path) -> Result<PdfPlan> {
    let Some(first) = infos.first() else {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "no PDFs to merge",
        ));
    };
    let out_name = display_name(output);
    let total: u32 = infos.iter().map(|i| i.pages).sum();

    let mut plan = PdfPlan::new(
        PdfJob::Merge {
            inputs: infos.iter().map(|i| i.path.clone()).collect(),
            passwords: infos.iter().map(|i| i.password.clone()).collect(),
            output: output.to_path_buf(),
        },
        vec![PlannedOutput {
            path: output.to_path_buf(),
            pages: (1..=total).collect(),
        }],
        out_name.clone(),
    );
    let dropped = names_where(&infos[1..], |i| i.has_bookmarks && i.path != first.path);
    if !dropped.is_empty() {
        let mut s = format!("Bookmarks from {} are not carried over", join_and(&dropped));
        if first.has_bookmarks {
            s.push_str(&format!(
                "; {out_name} keeps {}'s",
                display_name(&first.path)
            ));
        }
        s.push('.');
        plan.notes.push(s);
    }
    let restricted = names_where(infos, |i| i.restricted);
    if !restricted.is_empty() {
        let note = restricted_note(&restricted, &format!("{out_name} does not"));
        plan.notes.push(note.clone());
        plan.restricted_note = Some(note);
    }
    let locked = names_where(infos, PdfInfo::locked);
    if !locked.is_empty() {
        let note = unlocked_note(&locked, &format!("{out_name} isn't"), "it");
        plan.notes.push(note.clone());
        plan.unlocked_note = Some(note);
    }
    if infos.len() == 1 {
        plan.notes.push(format!(
            "Only one input, so {out_name} is a copy of {}.",
            display_name(&first.path)
        ));
    }

    let damaged = names_where(infos, |i| i.damaged);
    if !damaged.is_empty() {
        plan.warnings.push(repaired_warning(&damaged, &out_name));
        plan.repaired = true;
    }
    Ok(plan)
}

/// Plans a split of `info` into `dir`: one file per page without ranges,
/// one per range with them.
pub fn split_plan(info: &PdfInfo, ranges: &[PageRange], dir: &Path) -> Result<PdfPlan> {
    let name = display_name(&info.path);
    if info.pages == 0 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} has no pages"),
        ));
    }
    let stem = info
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_string());

    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    if info.has_bookmarks {
        notes.push(format!(
            "{name}'s bookmarks are not carried into the split files."
        ));
    }

    let (job, outputs) = if ranges.is_empty() {
        let width = info.pages.to_string().len();
        let outputs = (1..=info.pages)
            .map(|p| PlannedOutput {
                path: dir.join(format!("{stem}-{p:0width$}.pdf")),
                pages: vec![p],
            })
            .collect();
        let job = PdfJob::SplitPages {
            input: info.path.clone(),
            password: info.password.clone(),
            dir: dir.to_path_buf(),
            stem,
        };
        (job, outputs)
    } else {
        let resolved = ranges
            .iter()
            .map(|r| r.resolve(info.pages, &name))
            .collect::<Result<Vec<_>>>()?;
        let mut outputs: Vec<PlannedOutput> = Vec::with_capacity(resolved.len());
        for (i, r) in resolved.iter().enumerate() {
            let path = dir.join(format!("{stem}-{}.pdf", r.label()));
            if let Some(j) = outputs.iter().position(|o| o.path == path) {
                return Err(ConvError::new(
                    ErrorCode::InvalidInvocation,
                    format!(
                        "{} and {} would both write {}",
                        ranges[j].text,
                        ranges[i].text,
                        display_name(&path)
                    ),
                ));
            }
            outputs.push(PlannedOutput {
                path,
                pages: r.pages(),
            });
        }
        let (left_out, repeated) = coverage(info.pages, &resolved);
        if !repeated.is_empty() {
            notes.push(overlap_note(&repeated));
        }
        if !left_out.is_empty() {
            warnings.push(left_out_warning(&left_out));
        }
        let job = PdfJob::SplitRanges {
            input: info.path.clone(),
            password: info.password.clone(),
            ranges: resolved,
        };
        (job, outputs)
    };

    let mut plan = PdfPlan::new(job, outputs, "Each split file".to_string());
    plan.notes = notes;
    plan.warnings = warnings;
    source_notes(&mut plan, info, "the split files", true);
    Ok(plan)
}

/// Plans writing `info` again as `output`: unlocked, or ready for
/// `set_lock`. Unlocking a file that has nothing to unlock is refused.
pub fn rewrite_plan(info: &PdfInfo, output: &Path, rewrite: Rewrite) -> Result<PdfPlan> {
    let name = display_name(&info.path);
    let out_name = display_name(output);
    if rewrite == Rewrite::Unlock && !info.encrypted {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("{name} has no password or restrictions to remove"),
        ));
    }
    let mut plan = PdfPlan::new(
        PdfJob::Rewrite {
            input: info.path.clone(),
            password: info.password.clone(),
            output: output.to_path_buf(),
        },
        vec![PlannedOutput {
            path: output.to_path_buf(),
            pages: (1..=info.pages).collect(),
        }],
        out_name.clone(),
    );
    source_notes(&mut plan, info, &out_name, false);
    // Unlocking is the point, so the output lacking a password gets no note.
    if let Some(note) = plan.unlocked_note.take() {
        plan.notes.retain(|n| n != &note);
    }
    Ok(plan)
}

pub(crate) fn check_inputs<'a>(inputs: impl IntoIterator<Item = &'a Path>) -> Result<()> {
    for input in inputs {
        if !input.is_file() {
            return Err(ConvError::new(
                ErrorCode::InputNotFound,
                format!("input not found: {}", input.display()),
            ));
        }
    }
    Ok(())
}

/// Opens every input with qpdf (each distinct path once, asking `ask` for
/// any password) and plans the merge.
pub fn plan_merge(
    qpdf: &Path,
    inputs: &[PathBuf],
    output: &Path,
    ask: &mut AskPassword,
) -> Result<PdfPlan> {
    check_inputs(inputs.iter().map(PathBuf::as_path))?;
    let mut seen: Vec<PdfInfo> = Vec::new();
    let mut infos = Vec::with_capacity(inputs.len());
    for input in inputs {
        let info = match seen.iter().find(|i| &i.path == input) {
            Some(i) => i.clone(),
            None => {
                let i = read::open(qpdf, input, ask)?;
                seen.push(i.clone());
                i
            }
        };
        infos.push(info);
    }
    merge_plan(&infos, output)
}

/// Opens `input` with qpdf and plans the split, into `outdir` or next to
/// the input.
pub fn plan_split(
    qpdf: &Path,
    input: &Path,
    ranges: &[PageRange],
    outdir: Option<&Path>,
    ask: &mut AskPassword,
) -> Result<PdfPlan> {
    check_inputs([input])?;
    let info = read::open(qpdf, input, ask)?;
    let dir = match outdir {
        Some(d) => d.to_path_buf(),
        None => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    split_plan(&info, ranges, &dir)
}

/// Opens `input` with qpdf and plans writing it again as `output`.
pub fn plan_rewrite(
    qpdf: &Path,
    input: &Path,
    output: &Path,
    rewrite: Rewrite,
    ask: &mut AskPassword,
) -> Result<PdfPlan> {
    check_inputs([input])?;
    let info = read::open(qpdf, input, ask)?;
    rewrite_plan(&info, output, rewrite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::parse_range;

    fn info(path: &str, pages: u32) -> PdfInfo {
        PdfInfo {
            path: PathBuf::from(path),
            pages,
            has_bookmarks: false,
            bookmarked_pages: vec![],
            encrypted: false,
            restricted: false,
            password: None,
            damaged: false,
        }
    }

    fn locked(path: &str, pages: u32, password: &str) -> PdfInfo {
        with(info(path, pages), |i| {
            i.encrypted = true;
            i.password = Some(Password::new(password).unwrap());
        })
    }

    fn lock(password: &str) -> Lock {
        Lock::new(Password::new(password).unwrap(), &[]).unwrap()
    }

    fn with(mut i: PdfInfo, f: impl FnOnce(&mut PdfInfo)) -> PdfInfo {
        f(&mut i);
        i
    }

    fn ranges(texts: &[&str]) -> Vec<PageRange> {
        texts.iter().map(|t| parse_range(t).unwrap()).collect()
    }

    fn names(plan: &PdfPlan) -> Vec<String> {
        plan.outputs.iter().map(|o| display_name(&o.path)).collect()
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // --- merge ---

    #[test]
    fn a_merge_writes_one_file_with_every_page() {
        let plan = merge_plan(&[info("a.pdf", 3), info("b.pdf", 2)], Path::new("out.pdf")).unwrap();
        assert!(plan.is_merge());
        assert_eq!(
            plan.outputs,
            vec![PlannedOutput {
                path: "out.pdf".into(),
                pages: vec![1, 2, 3, 4, 5]
            }]
        );
        assert!(plan.notes.is_empty() && plan.warnings.is_empty() && !plan.repaired);
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "--decrypt",
                "a.pdf",
                "--pages",
                ".",
                "b.pdf",
                "--",
                "out.pdf"
            ])]
        );
    }

    #[test]
    fn a_merge_of_one_input_is_a_copy_with_a_note() {
        let plan = merge_plan(&[info("a.pdf", 3)], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.commands(),
            vec![argv(&["--decrypt", "a.pdf", "out.pdf"])]
        );
        assert_eq!(
            plan.notes,
            vec!["Only one input, so out.pdf is a copy of a.pdf."]
        );
    }

    #[test]
    fn a_merge_with_no_inputs_is_refused() {
        assert_eq!(
            merge_plan(&[], Path::new("out.pdf")).unwrap_err().code,
            ErrorCode::InvalidInvocation
        );
    }

    #[test]
    fn merge_bookmark_notes_name_the_later_inputs() {
        let b = |p: &str| with(info(p, 1), |i| i.has_bookmarks = true);
        let plan = merge_plan(&[b("a.pdf"), b("b.pdf"), b("c.pdf")], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.notes,
            vec!["Bookmarks from b.pdf and c.pdf are not carried over; out.pdf keeps a.pdf's."]
        );
        let plan = merge_plan(&[info("a.pdf", 1), b("b.pdf")], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.notes,
            vec!["Bookmarks from b.pdf are not carried over."]
        );
        let plan = merge_plan(&[b("a.pdf"), info("b.pdf", 1)], Path::new("out.pdf")).unwrap();
        assert!(plan.notes.is_empty(), "the first file's bookmarks are kept");
        let plan = merge_plan(
            &[b("a.pdf"), info("b.pdf", 1), b("a.pdf")],
            Path::new("out.pdf"),
        )
        .unwrap();
        assert!(
            plan.notes.is_empty(),
            "a.pdf's bookmarks are kept through its first copy"
        );
    }

    #[test]
    fn merge_notes_dropped_restrictions() {
        let r = |p: &str| with(info(p, 1), |i| i.restricted = true);
        let plan = merge_plan(&[r("a.pdf"), info("b.pdf", 1)], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.notes,
            vec!["a.pdf restricts printing, copying or editing; out.pdf does not."]
        );
        let plan = merge_plan(&[r("a.pdf"), r("b.pdf")], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.notes,
            vec!["a.pdf and b.pdf restrict printing, copying or editing; out.pdf does not."]
        );
    }

    #[test]
    fn merge_warns_about_repaired_inputs() {
        let d = |p: &str| with(info(p, 1), |i| i.damaged = true);
        let plan = merge_plan(&[info("a.pdf", 1), d("b.pdf")], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.warnings,
            vec!["b.pdf was damaged and has been repaired; check out.pdf."]
        );
        assert!(plan.repaired);
        let plan = merge_plan(&[d("a.pdf"), d("b.pdf")], Path::new("out.pdf")).unwrap();
        assert_eq!(
            plan.warnings,
            vec!["a.pdf and b.pdf were damaged and have been repaired; check out.pdf."]
        );
    }

    #[test]
    fn merge_arguments_survive_awkward_names() {
        let plan = merge_plan(
            &[info("-cover.pdf", 1), info("@b.pdf", 1)],
            Path::new("-out.pdf"),
        )
        .unwrap();
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "--decrypt",
                "./-cover.pdf",
                "--pages",
                ".",
                "./@b.pdf",
                "--",
                "./-out.pdf"
            ])]
        );
    }

    // --- split by page ---

    #[test]
    fn a_split_without_ranges_writes_one_padded_file_per_page() {
        let plan = split_plan(&info("report.pdf", 12), &[], Path::new("")).unwrap();
        assert!(!plan.is_merge());
        let n = names(&plan);
        assert_eq!(n.len(), 12);
        assert_eq!(
            (n[0].as_str(), n[11].as_str()),
            ("report-01.pdf", "report-12.pdf")
        );
        assert_eq!(plan.outputs[4].pages, vec![5]);
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "--decrypt",
                "--split-pages",
                "report.pdf",
                "report-%d.pdf"
            ])]
        );
    }

    #[test]
    fn padding_matches_the_page_counts_width() {
        let last = |pages| {
            names(&split_plan(&info("r.pdf", pages), &[], Path::new("")).unwrap())
                .pop()
                .unwrap()
        };
        let first = |pages| {
            names(&split_plan(&info("r.pdf", pages), &[], Path::new("")).unwrap()).remove(0)
        };
        assert_eq!(
            (first(9), last(9)),
            ("r-1.pdf".to_string(), "r-9.pdf".to_string())
        );
        assert_eq!(
            (first(10), last(10)),
            ("r-01.pdf".to_string(), "r-10.pdf".to_string())
        );
        assert_eq!(
            (first(100), last(100)),
            ("r-001.pdf".to_string(), "r-100.pdf".to_string())
        );
    }

    #[test]
    fn split_outputs_go_into_the_given_folder() {
        let plan = split_plan(&info("report.pdf", 2), &[], Path::new("pages")).unwrap();
        assert_eq!(
            plan.outputs[0].path,
            Path::new("pages").join("report-1.pdf")
        );
        assert_eq!(
            plan.commands()[0][3],
            Path::new("pages").join("report-%d.pdf").to_string_lossy()
        );
    }

    // --- split by range ---

    #[test]
    fn a_split_by_ranges_writes_one_file_per_range_and_warns_about_pages_left_out() {
        let plan = split_plan(
            &info("report.pdf", 12),
            &ranges(&["1-3", "4-10"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(names(&plan), vec!["report-1-3.pdf", "report-4-10.pdf"]);
        assert_eq!(plan.outputs[0].pages, vec![1, 2, 3]);
        assert_eq!(
            plan.warnings,
            vec!["Pages 11-12 are not in any range, so they were left out."]
        );
        assert!(plan.notes.is_empty());
        assert_eq!(
            plan.commands(),
            vec![
                argv(&[
                    "--decrypt",
                    "--empty",
                    "--pages",
                    "report.pdf",
                    "1-3",
                    "--",
                    "report-1-3.pdf"
                ]),
                argv(&[
                    "--decrypt",
                    "--empty",
                    "--pages",
                    "report.pdf",
                    "4-10",
                    "--",
                    "report-4-10.pdf"
                ]),
            ]
        );
    }

    #[test]
    fn z_is_written_as_the_last_page_and_reversed_ranges_reverse() {
        let plan = split_plan(
            &info("report.pdf", 12),
            &ranges(&["11-z", "5-1"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(names(&plan), vec!["report-11-12.pdf", "report-5-1.pdf"]);
        assert_eq!(plan.outputs[1].pages, vec![5, 4, 3, 2, 1]);
        assert_eq!(
            plan.warnings,
            vec!["Pages 6-10 are not in any range, so they were left out."]
        );
    }

    #[test]
    fn overlapping_ranges_get_a_note_and_single_pages_read_in_the_singular() {
        let plan = split_plan(
            &info("r.pdf", 12),
            &ranges(&["1-5", "3-8", "8-12"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(
            plan.notes,
            vec![
                "Pages 3-5 and 8 are in more than one range, so they appear in more than one file."
            ]
        );
        assert!(plan.warnings.is_empty());
        let plan = split_plan(&info("r.pdf", 12), &ranges(&["2-12"]), Path::new("")).unwrap();
        assert_eq!(
            plan.warnings,
            vec!["Page 1 is not in any range, so it was left out."]
        );
        let plan =
            split_plan(&info("r.pdf", 12), &ranges(&["1-3", "3-12"]), Path::new("")).unwrap();
        assert_eq!(
            plan.notes,
            vec!["Page 3 is in more than one range, so it appears in more than one file."]
        );
    }

    #[test]
    fn two_ranges_that_would_write_one_file_are_refused() {
        let e = split_plan(
            &info("report.pdf", 12),
            &ranges(&["11-z", "11-12"]),
            Path::new(""),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "11-z and 11-12 would both write report-11-12.pdf"
        );
    }

    #[test]
    fn a_range_past_the_end_and_an_empty_pdf_are_refused() {
        let e =
            split_plan(&info("report.pdf", 12), &ranges(&["15-20"]), Path::new("")).unwrap_err();
        assert_eq!(
            e.message,
            "report.pdf has 12 pages; 15-20 goes past the end"
        );
        let e = split_plan(&info("empty.pdf", 0), &[], Path::new("")).unwrap_err();
        assert_eq!(e.message, "empty.pdf has no pages");
    }

    #[test]
    fn split_notes_and_warnings_about_the_source() {
        let source = with(info("report.pdf", 2), |i| {
            i.has_bookmarks = true;
            i.restricted = true;
            i.damaged = true;
        });
        let plan = split_plan(&source, &[], Path::new("")).unwrap();
        assert_eq!(
            plan.notes,
            vec![
                "report.pdf's bookmarks are not carried into the split files.",
                "report.pdf restricts printing, copying or editing; the split files do not.",
            ]
        );
        assert_eq!(
            plan.warnings,
            vec!["report.pdf was damaged and has been repaired; check the split files."]
        );
        assert!(plan.repaired);
    }

    // --- scratch ---

    #[test]
    fn a_real_run_writes_into_scratch_under_fixed_names() {
        let s = Path::new("scratch");
        let merge =
            merge_plan(&[info("a.pdf", 1), info("b.pdf", 1)], Path::new("out.pdf")).unwrap();
        assert_eq!(
            merge.commands_into(Some(s), true)[0].last().unwrap(),
            &s.join("out.pdf").to_string_lossy()
        );
        assert_eq!(merge.scratch_output(s, 0), s.join("out.pdf"));

        let pages = split_plan(&info("report.pdf", 12), &[], Path::new("")).unwrap();
        assert_eq!(pages.commands_into(Some(s), true)[0][3], "p-%d.pdf");
        assert!(pages.runs_in_scratch());
        assert!(!merge.runs_in_scratch());
        assert_eq!(pages.scratch_output(s, 0), s.join("p-01.pdf"));
        assert_eq!(pages.scratch_output(s, 11), s.join("p-12.pdf"));

        let parts = split_plan(
            &info("report.pdf", 12),
            &ranges(&["1-3", "4"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(
            parts.commands_into(Some(s), true)[1].last().unwrap(),
            &s.join("r-2.pdf").to_string_lossy()
        );
        assert_eq!(parts.scratch_output(s, 1), s.join("r-2.pdf"));
    }

    // --- passwords ---

    #[test]
    fn a_merge_opens_each_locked_input_with_its_password_and_notes_the_result_is_not() {
        let plan = merge_plan(
            &[
                locked("a.pdf", 1, "pa"),
                info("b.pdf", 1),
                locked("c.pdf", 1, "pc"),
            ],
            Path::new("out.pdf"),
        )
        .unwrap();
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "--decrypt",
                "--password=<password>",
                "a.pdf",
                "--pages",
                ".",
                "b.pdf",
                "c.pdf",
                "--password=<password>",
                "--",
                "out.pdf"
            ])]
        );
        let real = plan.commands_into(None, true).remove(0);
        assert_eq!(real[1], "--password=pa");
        assert_eq!(real[7], "--password=pc");
        assert!(plan.has_secrets());
        assert_eq!(
            plan.notes,
            vec!["a.pdf and c.pdf were password-protected; out.pdf isn't. Add --password to lock it."]
        );
    }

    #[test]
    fn a_plan_without_passwords_has_no_secrets() {
        let plan = merge_plan(&[info("a.pdf", 1), info("b.pdf", 1)], Path::new("o.pdf")).unwrap();
        assert!(!plan.has_secrets());
        assert_eq!(plan.commands(), plan.commands_into(None, true));
    }

    #[test]
    fn a_lock_goes_on_every_output_in_place_of_decrypt() {
        let mut merge = merge_plan(
            &[locked("a.pdf", 1, "pa"), info("b.pdf", 1)],
            Path::new("out.pdf"),
        )
        .unwrap();
        merge.set_lock(lock("new"));
        assert!(merge.notes.is_empty(), "{:?}", merge.notes);
        assert_eq!(
            merge.commands(),
            vec![argv(&[
                "--password=<password>",
                "a.pdf",
                "--pages",
                ".",
                "b.pdf",
                "--",
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<new password>",
                "--bits=256",
                "--",
                "out.pdf"
            ])]
        );

        let mut pages = split_plan(&info("report.pdf", 2), &[], Path::new("")).unwrap();
        pages.set_lock(lock("new"));
        assert!(pages.has_secrets());
        assert_eq!(
            pages.commands()[0],
            argv(&[
                "--split-pages",
                "report.pdf",
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<new password>",
                "--bits=256",
                "--",
                "report-%d.pdf"
            ])
        );

        let mut parts = split_plan(
            &locked("report.pdf", 4, "p"),
            &ranges(&["1-2", "3-4"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(
            parts.notes,
            vec!["report.pdf was password-protected; the split files aren't. Add --password to lock them."]
        );
        parts.set_lock(
            Lock::new(
                Password::new("new").unwrap(),
                &[crate::pdf::Restriction::Print],
            )
            .unwrap(),
        );
        assert_eq!(
            parts.notes,
            vec![
                "Each split file opens with the password but turns off printing; PDF apps \
                  honour that, though nothing forces them to."
            ]
        );
        assert_eq!(
            parts.commands()[1],
            argv(&[
                "--empty",
                "--pages",
                "report.pdf",
                "--password=<password>",
                "3-4",
                "--",
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<random>",
                "--bits=256",
                "--print=none",
                "--",
                "report-3-4.pdf"
            ])
        );
    }

    #[test]
    fn a_restricted_lock_replaces_the_note_that_restrictions_were_dropped() {
        let source = with(info("r.pdf", 1), |i| {
            i.encrypted = true;
            i.restricted = true;
        });
        let mut plan = rewrite_plan(&source, Path::new("r-locked.pdf"), Rewrite::Lock).unwrap();
        plan.set_lock(lock("pw"));
        assert_eq!(
            plan.notes,
            vec!["r.pdf restricts printing, copying or editing; r-locked.pdf does not."],
            "without restrictions of its own, the lock drops them"
        );
        let mut plan = rewrite_plan(&source, Path::new("r-locked.pdf"), Rewrite::Lock).unwrap();
        plan.set_lock(
            Lock::new(
                Password::new("pw").unwrap(),
                &[crate::pdf::Restriction::Print],
            )
            .unwrap(),
        );
        assert_eq!(plan.notes.len(), 1, "{:?}", plan.notes);
        assert!(
            plan.notes[0].contains("turns off printing"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_password_preview_cannot_take_gets_a_warning() {
        let mut plan =
            rewrite_plan(&info("r.pdf", 1), Path::new("r-locked.pdf"), Rewrite::Lock).unwrap();
        plan.set_lock(lock("contraseña"));
        assert_eq!(
            plan.warnings,
            vec![
                "Apple's Preview can't open a PDF whose password has characters beyond plain \
                  ASCII, such as é or ü; most other PDF apps can."
            ]
        );
        let mut plan =
            rewrite_plan(&info("r.pdf", 1), Path::new("r-locked.pdf"), Rewrite::Lock).unwrap();
        plan.set_lock(lock("plain words & 123!"));
        assert!(plan.warnings.is_empty());
    }

    // --- rewrite ---

    #[test]
    fn an_unlock_writes_the_file_again_without_its_password() {
        let plan = rewrite_plan(
            &locked("report.pdf", 3, "p"),
            Path::new("report-unlocked.pdf"),
            Rewrite::Unlock,
        )
        .unwrap();
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "--decrypt",
                "--password=<password>",
                "report.pdf",
                "report-unlocked.pdf"
            ])]
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(plan.outputs[0].pages, vec![1, 2, 3]);
    }

    #[test]
    fn unlocking_a_restricted_file_says_the_restrictions_are_gone() {
        let source = with(info("report.pdf", 1), |i| {
            i.encrypted = true;
            i.restricted = true;
        });
        let plan = rewrite_plan(&source, Path::new("r-unlocked.pdf"), Rewrite::Unlock).unwrap();
        assert_eq!(
            plan.notes,
            vec!["report.pdf restricts printing, copying or editing; r-unlocked.pdf does not."]
        );
        assert!(!plan.has_secrets());
    }

    #[test]
    fn unlocking_a_file_with_nothing_to_unlock_is_refused() {
        let e =
            rewrite_plan(&info("report.pdf", 1), Path::new("x.pdf"), Rewrite::Unlock).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "report.pdf has no password or restrictions to remove"
        );
    }

    #[test]
    fn a_lock_rewrites_the_file_with_the_lock() {
        let mut plan = rewrite_plan(
            &info("report.pdf", 1),
            Path::new("report-locked.pdf"),
            Rewrite::Lock,
        )
        .unwrap();
        plan.set_lock(lock("pw"));
        assert_eq!(
            plan.commands(),
            vec![argv(&[
                "report.pdf",
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<new password>",
                "--bits=256",
                "--",
                "report-locked.pdf"
            ])]
        );
        let real = plan.commands_into(Some(Path::new("s")), true).remove(0);
        assert_eq!(real[1..3], ["--encrypt", "--user-password=pw"]);
        assert_eq!(
            real.last().unwrap(),
            &Path::new("s").join("out.pdf").to_string_lossy()
        );
    }
}
