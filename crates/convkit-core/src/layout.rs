//! The page layout Typst compiles to merge images into a PDF, and the
//! staging that puts the images where it can read them.
//!
//! Typst reads files only under its `--root`, and the main file must be
//! inside it too, while the inputs can be anywhere (on Windows, on another
//! drive). So each input is staged into the output's folder, which in a
//! real run is the per-run scratch dir, under a name convkit makes up, and
//! the layout names those files only. A user's file name never reaches the
//! layout, so it needs no escaping: quotes, `#`, `$` and non-ASCII names
//! cannot break it.
//!
//! Pure, like `plan::build`: this computes what `exec` will write and
//! stage (`plan::Prepare`), and touches nothing.

use std::path::{Path, PathBuf};

use crate::plan::Prepare;
use crate::Format;

/// The first line of every layout: each page takes the size of the image
/// on it, at the image's own DPI, with no margin.
const PAGE_SETUP: &str = "#set page(width: auto, height: auto, margin: 0pt)";

/// Where the layout for `output` is written: beside it, so inside the root
/// Typst is given (`Arg::OutDir`).
pub(crate) fn layout_path(output: &Path) -> PathBuf {
    output.with_extension("convkit-layout.typ")
}

/// The name input `index` (from 0) is staged under: numbered from 1, with
/// the canonical extension of the input's format. Typst picks its decoder
/// by extension and knows `.jpg` but not `.jpeg`, `.jpe` or `.jfif`, so
/// every spelling of a format stages as the one Typst reads.
pub(crate) fn staged_name(index: usize, input: &Path) -> String {
    // Every input on the Typst route has a format
    // (`registry::image_pdf_variant`); one without would meet Typst's own
    // "unknown image format", still under a name of convkit's.
    let ext = Format::from_path(input).map_or("unknown", |f| f.ext());
    format!("convkit-in-{:04}.{ext}", index + 1)
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

    /// Typst picks the decoder by extension and knows only the canonical
    /// ones, so every spelling of a format stages under its canonical one.
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
}
