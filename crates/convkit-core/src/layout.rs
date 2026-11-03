//! The page layout Typst compiles to merge images into a PDF, and the
//! staging that puts the images where it can read them.
//!
//! Typst reads files only under its `--root`, and the main file must be
//! inside it too, while the inputs can be anywhere (on Windows, on another
//! drive). So each input is staged into the output's folder, which in a
//! real run is the per-run scratch dir, under a name convkit makes up, and
//! the layout names those files only. A user's file name never reaches the
//! layout, so it needs no escaping: quotes, `#`, `$` and non-ASCII names
//! cannot break it. What Typst reports about those files is put back in
//! terms of the user's (`name_inputs`).
//!
//! Pure, like `plan::build`: this computes what `exec` will write and
//! stage (`plan::Prepare`), and touches nothing.

use std::path::{Path, PathBuf};

use crate::plan::{PlannedStep, Prepare};
use crate::Format;

/// The first line of every layout: each page takes the size of the image
/// on it, at the image's own DPI, with no margin.
const PAGE_SETUP: &str = "#set page(width: auto, height: auto, margin: 0pt)";

/// What every staged name starts with.
const STAGED: &str = "convkit-in-";

/// Where the layout for `output` is written: beside it, so inside the root
/// Typst is given (`Arg::OutDir`).
pub(crate) fn layout_path(output: &Path) -> PathBuf {
    output.with_extension("convkit-layout.typ")
}

/// The name input `index` (from 0) is staged under: numbered from 1, with
/// the canonical extension of the input's format, so each format has one
/// predictable name however the user spelled it (`.JPG`, `.jpeg`, `.jfif`
/// all stage as `.jpg`), and the known extension picks Typst's decoder
/// directly.
pub(crate) fn staged_name(index: usize, input: &Path) -> String {
    // Every input on the Typst route has a format
    // (`registry::image_pdf_variant`); one without still gets a name of
    // convkit's, never text from the user's.
    let ext = Format::from_path(input).map_or("unknown", |f| f.ext());
    format!("{STAGED}{:04}.{ext}", index + 1)
}

/// What exec does before Typst runs: stage each input, in order, beside
/// `output`, then write the layout that puts each on a page of its own.
/// Input *i* (from 0) is on the layout's line *i* + 2, which is how a Typst
/// error's line is traced back to the user's file.
///
/// With `strip`, a JPEG is staged as a copy without its metadata
/// (`jpeg::strip`). A PNG, WebP or GIF is linked either way: Typst decodes
/// it to pixels, so none of its metadata reaches the PDF.
pub(crate) fn image_pages(inputs: &[PathBuf], output: &Path, strip: bool) -> Vec<Prepare> {
    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut layout = format!("{PAGE_SETUP}\n");
    let mut prepare = Vec::with_capacity(inputs.len() + 1);
    for (i, input) in inputs.iter().enumerate() {
        let name = staged_name(i, input);
        // A path from `/` is from Typst's root, not the filesystem's.
        layout.push_str(&format!("#page(image(\"/{name}\"))\n"));
        prepare.push(Prepare::Stage {
            from: input.clone(),
            to: dir.join(name),
            strip: strip && Format::from_path(input) == Some(Format::Jpg),
        });
    }
    prepare.push(Prepare::Write {
        path: layout_path(output),
        contents: layout,
    });
    prepare
}

/// A Typst merge's report as the user should read it. Typst names the
/// files it was given, which are convkit's: a diagnostic's opening
/// `<layout>:<line>:<col>: ` becomes the input on that line, and a staged
/// copy's path or name becomes its input's. Any other step's report is
/// returned as it is.
pub(crate) fn name_inputs(report: &str, step: &PlannedStep) -> String {
    let layout = match step.prepare.last() {
        Some(Prepare::Write { path, .. }) if *path == layout_path(&step.output) => path,
        _ => return report.to_string(),
    };
    let Some(layout_name) = layout.file_name().map(|n| n.to_string_lossy()) else {
        return report.to_string();
    };
    let inputs: Vec<(String, &Path)> = step
        .prepare
        .iter()
        .filter_map(|p| match p {
            Prepare::Stage { from, to, .. } => Some((from.display().to_string(), to.as_path())),
            Prepare::Write { .. } => None,
        })
        .collect();
    let mut staged: Vec<(String, &str)> = Vec::with_capacity(inputs.len() * 2);
    for (shown, to) in &inputs {
        staged.push((to.display().to_string(), shown));
        if let Some(name) = to.file_name() {
            staged.push((name.to_string_lossy().into_owned(), shown));
        }
    }
    // Longest first, so a staged path is replaced whole, not just its name.
    staged.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    report
        .split_inclusive('\n')
        .map(|line| match diagnostic_at(line, &layout_name) {
            // Line 1 sets the page up; input i is on line i + 2.
            Some((row, rest)) => match row.checked_sub(2).and_then(|i| inputs.get(i)) {
                Some((shown, _)) => format!("{shown}: {}", replace_staged(rest, &staged)),
                None => replace_staged(rest, &staged),
            },
            None => replace_staged(line, &staged),
        })
        .collect()
}

/// The layout line a short-format diagnostic, `<path>:<line>:<col>: ...`,
/// points at, and the text after its location.
fn diagnostic_at<'a>(line: &'a str, layout_name: &str) -> Option<(usize, &'a str)> {
    let (_, after) = line.split_once(layout_name)?;
    let (row, after) = after.strip_prefix(':')?.split_once(':')?;
    let (col, rest) = after.split_once(": ")?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !(digits(row) && digits(col)) {
        return None;
    }
    Some((row.parse().ok()?, rest))
}

/// `text` with each staged path or name replaced by its input's, in one
/// pass, so an input that is itself named like a staged file is not
/// replaced again.
fn replace_staged(text: &str, staged: &[(String, &str)]) -> String {
    if !text.contains(STAGED) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        match staged
            .iter()
            .find(|(from, _)| rest.starts_with(from.as_str()))
        {
            Some((from, shown)) => {
                out.push_str(shown);
                rest = &rest[from.len()..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn stage(from: &str, to: &str, strip: bool) -> Prepare {
        Prepare::Stage {
            from: p(from),
            to: p(to),
            strip,
        }
    }

    fn layout_of(prepare: &[Prepare]) -> &str {
        match prepare.last() {
            Some(Prepare::Write { contents, .. }) => contents,
            other => panic!("the layout is written last: {other:?}"),
        }
    }

    #[test]
    fn the_layout_places_each_input_on_its_own_line() {
        let inputs = [p("a.jpg"), p("B.PNG"), p("c.jpeg")];
        let prepare = image_pages(&inputs, Path::new("dir/out.pdf"), false);
        assert_eq!(
            prepare,
            [
                stage("a.jpg", "dir/convkit-in-0001.jpg", false),
                stage("B.PNG", "dir/convkit-in-0002.png", false),
                stage("c.jpeg", "dir/convkit-in-0003.jpg", false),
                Prepare::Write {
                    path: p("dir/out.convkit-layout.typ"),
                    contents: "#set page(width: auto, height: auto, margin: 0pt)\n\
                               #page(image(\"/convkit-in-0001.jpg\"))\n\
                               #page(image(\"/convkit-in-0002.png\"))\n\
                               #page(image(\"/convkit-in-0003.jpg\"))\n"
                        .to_string(),
                },
            ]
        );
        // Input i (from 0) is on line i + 2, which is how a Typst error's
        // line is traced back to the user's file.
        for (i, input) in inputs.iter().enumerate() {
            let line = layout_of(&prepare).lines().nth(i + 1).unwrap();
            assert!(line.contains(&staged_name(i, input)), "{line}");
        }
    }

    /// Every spelling of a format stages under the one canonical name.
    #[test]
    fn every_spelling_of_a_format_stages_under_its_canonical_extension() {
        for (name, staged) in [
            ("IMG_1.JPG", "convkit-in-0001.jpg"),
            ("scan.jpeg", "convkit-in-0001.jpg"),
            ("old.jpe", "convkit-in-0001.jpg"),
            ("web.JFIF", "convkit-in-0001.jpg"),
            ("x.PNG", "convkit-in-0001.png"),
            ("y.WebP", "convkit-in-0001.webp"),
            ("z.gif", "convkit-in-0001.gif"),
        ] {
            assert_eq!(staged_name(0, Path::new(name)), staged, "{name}");
        }
        assert_eq!(staged_name(41, Path::new("a.png")), "convkit-in-0042.png");
    }

    #[test]
    fn strip_applies_to_jpeg_inputs_only() {
        let inputs = [p("a.jpg"), p("b.png"), p("c.JPG")];
        let strips: Vec<bool> = image_pages(&inputs, Path::new("out.pdf"), true)
            .iter()
            .filter_map(|step| match step {
                Prepare::Stage { strip, .. } => Some(*strip),
                Prepare::Write { .. } => None,
            })
            .collect();
        assert_eq!(strips, [true, false, true]);
    }

    /// `conv a.jpg a.jpg out.pdf` is two pages: each mention of the file is
    /// staged under a name of its own.
    #[test]
    fn the_same_input_twice_is_staged_twice() {
        let inputs = [p("a.jpg"), p("a.jpg")];
        let prepare = image_pages(&inputs, Path::new("out.pdf"), false);
        assert_eq!(
            prepare[..2],
            [
                stage("a.jpg", "./convkit-in-0001.jpg", false),
                stage("a.jpg", "./convkit-in-0002.jpg", false),
            ]
        );
        assert_eq!(
            layout_of(&prepare)
                .lines()
                .filter(|l| l.starts_with("#page("))
                .count(),
            2
        );
    }

    /// The layout holds no text from the user's file names, so it needs no
    /// escaping: quotes, `#`, `$` and non-ASCII cannot break it.
    #[test]
    fn odd_file_names_never_reach_the_layout() {
        let odd = p("photos/a \"b\" #c $d é.jpg");
        let prepare = image_pages(std::slice::from_ref(&odd), Path::new("out.pdf"), false);
        assert_eq!(
            prepare[0],
            stage("photos/a \"b\" #c $d é.jpg", "./convkit-in-0001.jpg", false)
        );
        let layout = layout_of(&prepare);
        for user_text in ["\"b\"", "#c", "$d", "é", "photos"] {
            assert!(!layout.contains(user_text), "{user_text} in {layout}");
        }
        assert!(layout.is_ascii(), "{layout}");
    }

    #[test]
    fn the_layout_sits_beside_the_output() {
        assert_eq!(
            layout_path(Path::new("scratch/out.pdf")),
            p("scratch/out.convkit-layout.typ")
        );
    }

    /// The Typst step that merges `inputs` into `scratch/out.pdf`.
    fn typst_step(inputs: &[PathBuf]) -> PlannedStep {
        let plan = crate::plan::build(
            Format::Jpg,
            Format::Pdf,
            inputs,
            Path::new("scratch/out.pdf"),
            None,
            None,
        )
        .unwrap();
        plan.steps.into_iter().next().unwrap()
    }

    /// Typst names the layout and the staged copies, which are convkit's;
    /// the user knows only their own files. However Typst spells the
    /// layout's path (relative to where it runs, through `..`, or on
    /// Windows), line 4 is the third input.
    #[test]
    fn typst_errors_name_the_users_file() {
        let inputs = [
            p("a.jpg"),
            p("b.png"),
            p("photos/c d.png"),
            p("e.jpg"),
            p("f.jpg"),
        ];
        let step = typst_step(&inputs);
        for layout in [
            "/private/var/folders/x/scratch/out.convkit-layout.typ",
            "../../tmp/out dir/scratch/out.convkit-layout.typ",
            "scratch/out.convkit-layout.typ",
            "C:\\Users\\Rick Xie\\scratch\\out.convkit-layout.typ",
        ] {
            let report =
                format!("{layout}:4:6: error: failed to decode image (Invalid PNG signature.)");
            assert_eq!(
                name_inputs(&report, &step),
                "photos/c d.png: error: failed to decode image (Invalid PNG signature.)"
            );
        }

        let Prepare::Stage { to: fifth, .. } = &step.prepare[4] else {
            panic!("the fifth input is staged fifth: {:?}", step.prepare);
        };
        let report = format!(
            "scratch/out.convkit-layout.typ:2:6: warning: a first\n\
             scratch/out.convkit-layout.typ:6:6: error: file not found (searched at {})\n\
             error: convkit-in-0002.png is not a picture\n\
             scratch/out.convkit-layout.typ:1:2: error: on no input's line\n",
            fifth.display()
        );
        let named = name_inputs(&report, &step);
        assert_eq!(
            named,
            "a.jpg: warning: a first\n\
             f.jpg: error: file not found (searched at f.jpg)\n\
             error: b.png is not a picture\n\
             error: on no input's line\n"
        );
        for scratch_name in ["convkit-in-", "convkit-layout"] {
            assert!(!named.contains(scratch_name), "{named}");
        }
    }

    /// A user's file named like a staged one is not taken for one once it
    /// is in the report.
    #[test]
    fn an_input_named_like_a_staged_file_is_named_once() {
        let step = typst_step(&[p("convkit-in-0002.png"), p("b.png")]);
        assert_eq!(
            name_inputs("convkit-in-0001.png and convkit-in-0002.png", &step),
            "convkit-in-0002.png and b.png"
        );
    }

    #[test]
    fn other_reports_are_unchanged() {
        let report = "scratch/out.convkit-layout.typ:4:6: error: convkit-in-0001.png";
        let magick = crate::plan::build(
            Format::Png,
            Format::Jpg,
            &[p("a.png")],
            Path::new("scratch/out.jpg"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(name_inputs(report, &magick.steps[0]), report);

        let typst = typst_step(&[p("a.jpg")]);
        let plain = "warning: something general\nerror: and something else\n";
        assert_eq!(name_inputs(plain, &typst), plain);
        assert_eq!(name_inputs("", &typst), "");
    }
}
