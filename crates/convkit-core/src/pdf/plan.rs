//! Turning inputs and ranges into qpdf calls, the files they write, and
//! what conv should tell the person about the result.

use std::path::{Path, PathBuf};

use super::range::{coverage, join_and, spans, PageRange, Resolved};
use super::read::{self, PdfInfo};
use super::{display_name, path_arg};
use crate::error::{ConvError, ErrorCode, Result};

/// What a plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfJob {
    /// The first input is qpdf's primary input, so its bookmarks, metadata
    /// and form survive; see spec §4.1.
    Merge {
        inputs: Vec<PathBuf>,
        output: PathBuf,
    },
    /// One qpdf `--split-pages` pass, numbering files as qpdf does.
    SplitPages {
        input: PathBuf,
        dir: PathBuf,
        stem: String,
    },
    /// One qpdf call per range, each from an empty document so no bookmark
    /// points at a page the output lacks.
    SplitRanges {
        input: PathBuf,
        ranges: Vec<Resolved>,
    },
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
}

/// Where a merge writes inside the scratch folder.
const MERGE_SCRATCH: &str = "merged.pdf";

impl PdfPlan {
    pub fn is_merge(&self) -> bool {
        matches!(self.job, PdfJob::Merge { .. })
    }

    /// qpdf's arguments for each call, writing the final outputs: what
    /// `--dry-run` prints.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands_into(None)
    }

    /// The same calls writing into `scratch` instead, as `run` makes them.
    pub(crate) fn commands_into(&self, scratch: Option<&Path>) -> Vec<Vec<String>> {
        match &self.job {
            PdfJob::Merge { inputs, output } => {
                let out = scratch.map_or_else(|| output.clone(), |s| s.join(MERGE_SCRATCH));
                let mut argv = vec!["--decrypt".to_string(), path_arg(&inputs[0])];
                if inputs.len() > 1 {
                    argv.push("--pages".to_string());
                    argv.push(".".to_string());
                    argv.extend(inputs[1..].iter().map(|p| path_arg(p)));
                    argv.push("--".to_string());
                }
                argv.push(path_arg(&out));
                vec![argv]
            }
            PdfJob::SplitPages { input, dir, stem } => {
                let template = match scratch {
                    Some(s) => s.join("p-%d.pdf"),
                    None => dir.join(format!("{stem}-%d.pdf")),
                };
                vec![vec![
                    "--decrypt".to_string(),
                    "--split-pages".to_string(),
                    path_arg(input),
                    path_arg(&template),
                ]]
            }
            PdfJob::SplitRanges { input, ranges } => ranges
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let out = match scratch {
                        Some(s) => s.join(format!("r-{}.pdf", i + 1)),
                        None => self.outputs[i].path.clone(),
                    };
                    vec![
                        "--decrypt".to_string(),
                        "--empty".to_string(),
                        "--pages".to_string(),
                        path_arg(input),
                        r.label(),
                        "--".to_string(),
                        path_arg(&out),
                    ]
                })
                .collect(),
        }
    }

    /// Where output `i` lands inside `scratch` when the commands write
    /// there. For `SplitPages` this follows qpdf's own `%d` padding: the
    /// width of the page count.
    #[allow(dead_code)] // used once plans are run
    pub(crate) fn scratch_output(&self, scratch: &Path, i: usize) -> PathBuf {
        match &self.job {
            PdfJob::Merge { .. } => scratch.join(MERGE_SCRATCH),
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

fn restricted_note(names: &[String], result: &str) -> String {
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

fn repaired_warning(names: &[String], check: &str) -> String {
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

    let mut notes = Vec::new();
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
        notes.push(s);
    }
    let restricted = names_where(infos, |i| i.restricted);
    if !restricted.is_empty() {
        notes.push(restricted_note(
            &restricted,
            &format!("{out_name} does not"),
        ));
    }
    if infos.len() == 1 {
        notes.push(format!(
            "Only one input, so {out_name} is a copy of {}.",
            display_name(&first.path)
        ));
    }

    let damaged = names_where(infos, |i| i.damaged);
    let warnings = if damaged.is_empty() {
        Vec::new()
    } else {
        vec![repaired_warning(&damaged, &out_name)]
    };

    Ok(PdfPlan {
        job: PdfJob::Merge {
            inputs: infos.iter().map(|i| i.path.clone()).collect(),
            output: output.to_path_buf(),
        },
        outputs: vec![PlannedOutput {
            path: output.to_path_buf(),
            pages: (1..=total).collect(),
        }],
        notes,
        warnings,
        repaired: !damaged.is_empty(),
    })
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
    if info.restricted {
        notes.push(restricted_note(
            std::slice::from_ref(&name),
            "the split files do not",
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
            ranges: resolved,
        };
        (job, outputs)
    };

    if info.damaged {
        warnings.push(repaired_warning(
            std::slice::from_ref(&name),
            "the split files",
        ));
    }
    Ok(PdfPlan {
        job,
        outputs,
        notes,
        warnings,
        repaired: info.damaged,
    })
}

fn check_inputs<'a>(inputs: impl IntoIterator<Item = &'a Path>) -> Result<()> {
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

/// Reads every input with qpdf (each distinct path once) and plans the
/// merge.
pub fn plan_merge(qpdf: &Path, inputs: &[PathBuf], output: &Path) -> Result<PdfPlan> {
    check_inputs(inputs.iter().map(PathBuf::as_path))?;
    let mut seen: Vec<PdfInfo> = Vec::new();
    let mut infos = Vec::with_capacity(inputs.len());
    for input in inputs {
        let info = match seen.iter().find(|i| &i.path == input) {
            Some(i) => i.clone(),
            None => {
                let i = read::read(qpdf, input)?;
                seen.push(i.clone());
                i
            }
        };
        infos.push(info);
    }
    merge_plan(&infos, output)
}

/// Reads `input` with qpdf and plans the split, into `outdir` or next to
/// the input.
pub fn plan_split(
    qpdf: &Path,
    input: &Path,
    ranges: &[PageRange],
    outdir: Option<&Path>,
) -> Result<PdfPlan> {
    check_inputs([input])?;
    let info = read::read(qpdf, input)?;
    let dir = match outdir {
        Some(d) => d.to_path_buf(),
        None => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    split_plan(&info, ranges, &dir)
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
            restricted: false,
            damaged: false,
        }
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
            merge.commands_into(Some(s))[0].last().unwrap(),
            &s.join("merged.pdf").to_string_lossy()
        );
        assert_eq!(merge.scratch_output(s, 0), s.join("merged.pdf"));

        let pages = split_plan(&info("report.pdf", 12), &[], Path::new("")).unwrap();
        assert_eq!(
            pages.commands_into(Some(s))[0][3],
            s.join("p-%d.pdf").to_string_lossy()
        );
        assert_eq!(pages.scratch_output(s, 0), s.join("p-01.pdf"));
        assert_eq!(pages.scratch_output(s, 11), s.join("p-12.pdf"));

        let parts = split_plan(
            &info("report.pdf", 12),
            &ranges(&["1-3", "4"]),
            Path::new(""),
        )
        .unwrap();
        assert_eq!(
            parts.commands_into(Some(s))[1].last().unwrap(),
            &s.join("r-2.pdf").to_string_lossy()
        );
        assert_eq!(parts.scratch_output(s, 1), s.join("r-2.pdf"));
    }
}
