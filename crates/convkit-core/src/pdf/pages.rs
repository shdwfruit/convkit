//! `conv pages`: keep, reorder, delete and turn a PDF's pages, into one
//! file. Every page number means the input's page, whichever of the three
//! it appears in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::display_name;
use super::plan::{check_inputs, source_notes, PdfJob, PdfPlan, PlannedOutput};
use super::range::{ordered_runs, parse_list, spans, PageRange};
use super::read::{self, AskPassword, PdfInfo};
use crate::error::{ConvError, ErrorCode, Result};
use crate::rotate::Turn;

/// What to do to the pages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageEdit {
    /// The pages to write, in order, repeats allowed; empty is every page.
    pub keep: Vec<PageRange>,
    /// Pages to leave out.
    pub delete: Vec<PageRange>,
    /// Each turn with the pages it applies to, `None` for every page. Turns
    /// of one page add up.
    pub turns: Vec<(Turn, Option<Vec<PageRange>>)>,
}

/// The most a derived name says about the pages before it gives up and
/// says `pages`: past this, the list is no use as a name and risks the
/// file system's limit.
const MAX_LABEL: usize = 40;

/// Parses `--rotate`'s value for `conv pages`: a turn (`right`, `left`,
/// `180`, `90`, `270`, `-90`), alone for every page or with the pages after
/// a colon, `right:2,4`.
pub fn parse_turn(text: &str) -> Result<(Turn, Option<Vec<PageRange>>)> {
    let invalid = |m: String| ConvError::new(ErrorCode::InvalidInvocation, m);
    let (turn, pages) = match text.split_once(':') {
        Some((turn, pages)) => (turn, Some(pages)),
        None => (text, None),
    };
    let turn = crate::rotate::parse(turn).map_err(|e| {
        invalid(format!(
            "{e}; give the pages after a colon, as in right:2,4, or none for every page"
        ))
    })?;
    let pages = pages.map(parse_list).transpose()?;
    Ok((turn, pages))
}

/// Every page `ranges` name, resolved against `info`, in order.
fn resolve_all(ranges: &[PageRange], info: &PdfInfo) -> Result<Vec<u32>> {
    let name = display_name(&info.path);
    let mut pages = Vec::new();
    for r in ranges {
        pages.extend(r.resolve(info.pages, &name)?.pages());
    }
    Ok(pages)
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

fn page_or_pages(pages: &[u32]) -> String {
    match pages {
        [one] => format!("Page {one} is"),
        _ => format!("Pages {} are", spans(pages)),
    }
}

/// Plans `edit` on `info`, writing `output`, or a name made from the
/// input's and what changed in `dir`: `report-1-3.pdf` for the pages it
/// keeps, in order (`report-3_1-2_5-12.pdf`), `report-rotated.pdf` for a
/// turn, both for both (`report-1-3-rotated.pdf`).
pub fn pages_plan(
    info: &PdfInfo,
    edit: &PageEdit,
    output: Option<&Path>,
    dir: &Path,
) -> Result<PdfPlan> {
    let name = display_name(&info.path);
    if info.pages == 0 {
        return Err(invalid(format!("{name} has no pages")));
    }
    let all: Vec<u32> = (1..=info.pages).collect();
    let selected = if edit.keep.is_empty() {
        all.clone()
    } else {
        resolve_all(&edit.keep, info)?
    };
    let mut deleted = resolve_all(&edit.delete, info)?;
    deleted.sort_unstable();
    deleted.dedup();
    let result: Vec<u32> = selected
        .iter()
        .copied()
        .filter(|p| deleted.binary_search(p).is_err())
        .collect();
    if result.is_empty() {
        return Err(invalid(format!(
            "that leaves none of {name}'s pages; a PDF needs at least one"
        )));
    }

    let mut warnings = Vec::new();
    let unselected: Vec<u32> = deleted
        .iter()
        .copied()
        .filter(|p| !selected.contains(p))
        .collect();
    if !unselected.is_empty() {
        warnings.push(format!(
            "{} not among the pages you listed, so deleting {} changes nothing.",
            page_or_pages(&unselected),
            if unselected.len() == 1 { "it" } else { "them" }
        ));
    }

    // The net turn of each source page, in clockwise degrees.
    let mut net: BTreeMap<u32, u16> = BTreeMap::new();
    let mut turned = Vec::new();
    for (turn, on) in &edit.turns {
        // A turn without pages turns every page written, so it never
        // reaches one left out.
        let mut pages = match on {
            Some(ranges) => resolve_all(ranges, info)?,
            None => result.clone(),
        };
        pages.sort_unstable();
        pages.dedup();
        for p in pages {
            let d = net.entry(p).or_insert(0);
            *d = (*d + turn.clockwise()) % 360;
            turned.push(p);
        }
    }
    turned.sort_unstable();
    turned.dedup();
    let unturned: Vec<u32> = turned
        .iter()
        .copied()
        .filter(|p| !result.contains(p))
        .collect();
    if !unturned.is_empty() {
        warnings.push(format!(
            "{} not in the result, so turning {} changes nothing.",
            page_or_pages(&unturned),
            if unturned.len() == 1 { "it" } else { "them" }
        ));
    }
    // Each angle, and the output pages it turns, as qpdf numbers them.
    let mut by_angle: BTreeMap<u16, Vec<u32>> = BTreeMap::new();
    for (i, p) in result.iter().enumerate() {
        if let Some(&d) = net.get(p).filter(|&&d| d != 0) {
            by_angle.entry(d).or_default().push(i as u32 + 1);
        }
    }
    let turns: Vec<(u16, Vec<u32>)> = by_angle.into_iter().collect();

    let reordered = result != all;
    if !reordered && turns.is_empty() {
        return Err(invalid(format!(
            "that leaves {name}'s pages as they are; list the pages to keep, or use \
             --delete or --rotate"
        )));
    }

    let output = match output {
        Some(p) => p.to_path_buf(),
        None => {
            let stem = info
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "pages".to_string());
            let mut parts = Vec::new();
            if reordered {
                let label = ordered_runs(&result).join("_");
                parts.push(if label.len() > MAX_LABEL {
                    "pages".to_string()
                } else {
                    label
                });
            }
            if !turns.is_empty() {
                parts.push("rotated".to_string());
            }
            dir.join(format!("{stem}-{}.pdf", parts.join("-")))
        }
    };
    let out_name = display_name(&output);

    let mut plan = PdfPlan::new(
        PdfJob::Pages {
            input: info.path.clone(),
            password: info.password.clone(),
            output: output.clone(),
            pages: reordered.then(|| result.clone()),
            turns,
        },
        vec![PlannedOutput {
            path: output,
            pages: result.clone(),
        }],
        out_name.clone(),
    );
    let lost: usize = info
        .bookmarked_pages
        .iter()
        .filter(|p| !result.contains(p))
        .count();
    match lost {
        0 => {}
        1 => plan.notes.push(
            "1 bookmark led to a page that was left out; it is still listed but goes nowhere."
                .to_string(),
        ),
        n => plan.notes.push(format!(
            "{n} bookmarks led to pages that were left out; they are still listed but go \
             nowhere."
        )),
    }
    plan.warnings = warnings;
    source_notes(&mut plan, info, &out_name, false);
    Ok(plan)
}

/// Opens `input` with qpdf and plans `edit`, into `output` or a derived
/// name in `outdir` (next to the input without it).
pub fn plan_pages(
    qpdf: &Path,
    input: &Path,
    edit: &PageEdit,
    output: Option<&Path>,
    outdir: Option<&Path>,
    ask: &mut AskPassword,
) -> Result<PdfPlan> {
    check_inputs([input])?;
    let info = read::open(qpdf, input, ask)?;
    let dir: PathBuf = match outdir {
        Some(d) => d.to_path_buf(),
        None => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    pages_plan(&info, edit, output, &dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::parse_range;

    fn info(pages: u32) -> PdfInfo {
        PdfInfo {
            path: PathBuf::from("report.pdf"),
            pages,
            has_bookmarks: false,
            bookmarked_pages: vec![],
            encrypted: false,
            restricted: false,
            password: None,
            damaged: false,
        }
    }

    fn ranges(texts: &[&str]) -> Vec<PageRange> {
        texts.iter().map(|t| parse_range(t).unwrap()).collect()
    }

    fn keep(texts: &[&str]) -> PageEdit {
        PageEdit {
            keep: ranges(texts),
            ..PageEdit::default()
        }
    }

    fn plan(info: &PdfInfo, edit: &PageEdit) -> PdfPlan {
        pages_plan(info, edit, None, Path::new("")).unwrap()
    }

    fn name(plan: &PdfPlan) -> String {
        display_name(&plan.outputs[0].path)
    }

    fn argv(plan: &PdfPlan) -> Vec<String> {
        plan.commands().remove(0)
    }

    #[test]
    fn turns_parse_alone_or_with_pages() {
        assert_eq!(parse_turn("right").unwrap(), (Turn::Right, None));
        assert_eq!(parse_turn("-90").unwrap(), (Turn::Left, None));
        let (turn, pages) = parse_turn("180:2,4-z").unwrap();
        assert_eq!(turn, Turn::Half);
        let texts: Vec<_> = pages.unwrap().into_iter().map(|r| r.text).collect();
        assert_eq!(texts, vec!["2", "4-z"]);
        let e = parse_turn("up:2").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.message.starts_with("invalid turn 'up'") && e.message.ends_with("for every page"),
            "{}",
            e.message
        );
        let e = parse_turn("right:").unwrap_err();
        assert!(
            e.message.starts_with("`` is not a page list"),
            "{}",
            e.message
        );
        let e = parse_turn("right:2-").unwrap_err();
        assert!(
            e.message.starts_with("`2-` is not a page range"),
            "{}",
            e.message
        );
    }

    #[test]
    fn listed_pages_are_written_in_their_order_and_named_after_it() {
        let p = plan(&info(12), &keep(&["3", "1-2", "5-z"]));
        assert_eq!(p.outputs[0].pages, vec![3, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(name(&p), "report-3_1-2_5-12.pdf");
        assert_eq!(
            argv(&p),
            vec![
                "--decrypt",
                "report.pdf",
                "--pages",
                ".",
                "3,1-2,5-12",
                "--",
                "report-3_1-2_5-12.pdf"
            ]
        );
        assert!(p.notes.is_empty() && p.warnings.is_empty());
        // One range is named the way split names its file.
        assert_eq!(name(&plan(&info(12), &keep(&["1-3"]))), "report-1-3.pdf");
        assert_eq!(name(&plan(&info(12), &keep(&["5-1"]))), "report-5-1.pdf");
        let p = plan(&info(3), &keep(&["1", "1", "1"]));
        assert_eq!(p.outputs[0].pages, vec![1, 1, 1]);
        assert_eq!(name(&p), "report-1_1_1.pdf");
    }

    #[test]
    fn deleting_names_the_pages_that_are_left() {
        let edit = PageEdit {
            delete: ranges(&["4", "6-7"]),
            ..PageEdit::default()
        };
        let p = plan(&info(10), &edit);
        assert_eq!(p.outputs[0].pages, vec![1, 2, 3, 5, 8, 9, 10]);
        assert_eq!(name(&p), "report-1-3_5_8-10.pdf");
        assert_eq!(argv(&p)[4], "1-3,5,8-10");
    }

    #[test]
    fn a_selection_and_a_delete_combine() {
        let edit = PageEdit {
            keep: ranges(&["1-5"]),
            delete: ranges(&["3", "9"]),
            ..PageEdit::default()
        };
        let p = plan(&info(10), &edit);
        assert_eq!(p.outputs[0].pages, vec![1, 2, 4, 5]);
        assert_eq!(
            p.warnings,
            vec!["Page 9 is not among the pages you listed, so deleting it changes nothing."]
        );
    }

    #[test]
    fn a_turn_alone_keeps_every_page_and_names_the_file_rotated() {
        let edit = PageEdit {
            turns: vec![(Turn::Right, None)],
            ..PageEdit::default()
        };
        let p = plan(&info(3), &edit);
        assert_eq!(name(&p), "report-rotated.pdf");
        assert_eq!(
            argv(&p),
            vec![
                "--decrypt",
                "report.pdf",
                "--rotate=+90:1-3",
                "report-rotated.pdf"
            ]
        );
    }

    #[test]
    fn turns_follow_their_pages_into_the_new_order_and_add_up() {
        let edit = PageEdit {
            keep: ranges(&["3", "1-2"]),
            turns: vec![
                (Turn::Right, Some(ranges(&["1", "3"]))),
                (Turn::Half, Some(ranges(&["3"]))),
                (Turn::Left, Some(ranges(&["2"]))),
            ],
            ..PageEdit::default()
        };
        let p = plan(&info(3), &edit);
        // Source 3 is output 1 (90 + 180), source 1 is output 2 (90),
        // source 2 is output 3 (270).
        assert_eq!(
            argv(&p),
            vec![
                "--decrypt",
                "report.pdf",
                "--pages",
                ".",
                "3,1-2",
                "--",
                "--rotate=+90:2",
                "--rotate=+270:1,3",
                "report-3_1-2-rotated.pdf"
            ]
        );
        // A turn and its opposite cancel out.
        let edit = PageEdit {
            keep: ranges(&["2-3"]),
            turns: vec![
                (Turn::Right, Some(ranges(&["2"]))),
                (Turn::Left, Some(ranges(&["2"]))),
            ],
            ..PageEdit::default()
        };
        let p = plan(&info(3), &edit);
        assert_eq!(name(&p), "report-2-3.pdf");
        assert!(!argv(&p).iter().any(|a| a.starts_with("--rotate")));
    }

    #[test]
    fn a_turn_of_every_page_turns_only_the_pages_written() {
        let edit = PageEdit {
            keep: ranges(&["2-3"]),
            turns: vec![(Turn::Left, None)],
            ..PageEdit::default()
        };
        let p = plan(&info(3), &edit);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(argv(&p)[6], "--rotate=+270:1-2");
    }

    #[test]
    fn a_turned_page_that_is_left_out_gets_a_warning() {
        let edit = PageEdit {
            delete: ranges(&["2"]),
            turns: vec![(Turn::Right, Some(ranges(&["1-2"])))],
            ..PageEdit::default()
        };
        let p = plan(&info(3), &edit);
        assert_eq!(
            p.warnings,
            vec!["Page 2 is not in the result, so turning it changes nothing."]
        );
        assert_eq!(
            argv(&p)[5..],
            ["--", "--rotate=+90:1", "report-1_3-rotated.pdf"]
        );
    }

    #[test]
    fn nothing_to_do_and_nothing_left_are_refused() {
        let e = pages_plan(&info(3), &keep(&["1-z"]), None, Path::new("")).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(e
            .message
            .starts_with("that leaves report.pdf's pages as they are"));
        let edit = PageEdit {
            delete: ranges(&["1-z"]),
            ..PageEdit::default()
        };
        let e = pages_plan(&info(3), &edit, None, Path::new("")).unwrap_err();
        assert_eq!(
            e.message,
            "that leaves none of report.pdf's pages; a PDF needs at least one"
        );
        let e = pages_plan(&info(3), &keep(&["2-5"]), None, Path::new("")).unwrap_err();
        assert_eq!(e.message, "report.pdf has 3 pages; 2-5 goes past the end");
        let edit = PageEdit {
            turns: vec![(Turn::Right, Some(ranges(&["4"])))],
            ..PageEdit::default()
        };
        let e = pages_plan(&info(3), &edit, None, Path::new("")).unwrap_err();
        assert_eq!(e.message, "report.pdf has 3 pages; 4 goes past the end");
    }

    #[test]
    fn a_long_list_is_named_pages_and_a_named_output_is_kept() {
        let odd: Vec<String> = (1..=40).step_by(2).map(|p| p.to_string()).collect();
        let odd: Vec<&str> = odd.iter().map(String::as_str).collect();
        assert_eq!(name(&plan(&info(40), &keep(&odd))), "report-pages.pdf");
        let p = pages_plan(
            &info(3),
            &keep(&["2"]),
            Some(Path::new("cover.pdf")),
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(p.outputs[0].path, PathBuf::from("cover.pdf"));
        let p = pages_plan(&info(3), &keep(&["2"]), None, Path::new("out")).unwrap();
        assert_eq!(p.outputs[0].path, Path::new("out").join("report-2.pdf"));
    }

    #[test]
    fn bookmarks_to_pages_left_out_are_noted() {
        let mut source = info(3);
        source.has_bookmarks = true;
        source.bookmarked_pages = vec![1, 2, 3, 2];
        let p = plan(&source, &keep(&["1", "3"]));
        assert_eq!(
            p.notes,
            vec![
                "2 bookmarks led to pages that were left out; they are still listed but go nowhere."
            ]
        );
        let p = plan(&source, &keep(&["1-2"]));
        assert_eq!(
            p.notes,
            vec![
                "1 bookmark led to a page that was left out; it is still listed but goes nowhere."
            ]
        );
    }

    #[test]
    fn a_locked_source_is_opened_with_its_password_and_the_result_is_not_locked() {
        let mut source = info(3);
        source.encrypted = true;
        source.password = Some(crate::pdf::Password::new("s3cret").unwrap());
        let mut p = plan(&source, &keep(&["2"]));
        assert_eq!(
            argv(&p),
            vec![
                "--decrypt",
                "--password=<password>",
                "report.pdf",
                "--pages",
                ".",
                "2",
                "--",
                "report-2.pdf"
            ]
        );
        assert_eq!(
            p.commands_into(None, true)[0][1],
            "--password=s3cret",
            "a real run has the password"
        );
        assert_eq!(
            p.notes,
            vec![
                "report.pdf was password-protected; report-2.pdf isn't. Add --password to lock it."
            ]
        );
        p.set_lock(crate::pdf::Lock::new(crate::pdf::Password::new("new").unwrap(), &[]).unwrap());
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert_eq!(
            argv(&p),
            vec![
                "--password=<password>",
                "report.pdf",
                "--pages",
                ".",
                "2",
                "--",
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<new password>",
                "--bits=256",
                "--",
                "report-2.pdf"
            ]
        );
    }
}
