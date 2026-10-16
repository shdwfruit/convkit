//! A workbook laid out for PDF before LibreOffice prints it.
//!
//! LibreOffice prints a workbook the way its columns and pages are set, and
//! a workbook made in Excel often prints badly that way: a number in a
//! column too narrow for it prints as `###`, and a sheet wider than the page
//! is cut into strips across several pages. So rather than `--convert-to
//! pdf`, conv has LibreOffice run a Basic macro, `sheet_pdf.bas`, that
//! widens the columns whose numbers would print as `###`, scales a sheet
//! wider than its page down to the page width, exports the PDF, and writes
//! down what it changed for the notes.
//!
//! The macro measures each value on the same device LibreOffice lays the
//! printed page out with, so a column is widened exactly when it would have
//! printed `###`. Only those columns change, and a sheet the workbook
//! already scales, or gives a print range, is left as the workbook has it.
//!
//! The macro lives in the run's own LibreOffice profile, written there
//! before soffice starts (`seed`), and is called on the command line by its
//! URL with the input and output paths (`call`), which is what `--dry-run`
//! shows.

use std::io;
use std::path::Path;

/// The Basic source of the macro.
const SOURCE: &str = include_str!("sheet_pdf.bas");

/// The macro's URL up to its arguments: library `Convkit`, module `Sheets`.
const CALL: &str = "macro:///Convkit.Sheets.ToPdf(";

/// What the macro writes into the profile's `user` directory.
const REPORT: &str = "convkit-sheets.txt";

/// The note for a workbook printed as it is set after all.
pub(crate) const UNFITTED_NOTE: &str = "Columns and page widths are left as the workbook sets \
     them; LibreOffice could not adjust them.";

/// The command-line argument that has the macro export `input` to `output`.
///
/// LibreOffice decodes `%XX` in a macro URL before it reads the arguments,
/// and the arguments are Basic strings, so a `%` is written `%25` and a `"`
/// doubled. Nothing else in a path needs escaping: commas, brackets, spaces
/// and non-ASCII names all arrive as written.
pub(crate) fn call(input: &str, output: &str) -> String {
    let arg = |s: &str| s.replace('%', "%25").replace('"', "\"\"");
    format!("{CALL}\"{}\",\"{}\")", arg(input), arg(output))
}

/// `argv` with the macro handed absolute paths. soffice runs in conv's
/// working directory, but a relative path would be resolved by the macro,
/// against whatever LibreOffice takes that directory to be; an absolute one
/// leaves nothing to resolve.
pub(crate) fn with_absolute_paths(
    argv: &[String],
    input: &Path,
    output: &Path,
) -> io::Result<Vec<String>> {
    let input = std::path::absolute(input)?;
    let output = std::path::absolute(output)?;
    Ok(argv
        .iter()
        .map(|a| {
            if a.starts_with(CALL) {
                call(&input.to_string_lossy(), &output.to_string_lossy())
            } else {
                a.clone()
            }
        })
        .collect())
}

/// Whether a soffice step runs the macro, so its profile needs `seed`.
pub(crate) fn is_call(argv: &[String]) -> bool {
    argv.iter().any(|a| a.starts_with(CALL))
}

/// Writes the macro into a new LibreOffice profile, before soffice is
/// started with it.
///
/// On first start LibreOffice fills a new profile from its own presets,
/// overwriting the Basic library list with one naming only `Standard`. The
/// profile's settings say whether that has happened, so these say it has,
/// and LibreOffice keeps the list written here. Nothing in the presets
/// bears on a PDF export.
pub(crate) fn seed(profile: &Path) -> io::Result<()> {
    let user = profile.join("user");
    let library = user.join("basic").join("Convkit");
    std::fs::create_dir_all(&library)?;
    std::fs::write(
        user.join("registrymodifications.xcu"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<item oor:path="/org.openoffice.Setup/Office"><prop oor:name="ooSetupInstCompleted" oor:op="fuse"><value>true</value></prop></item>
</oor:items>
"#,
    )?;
    std::fs::write(
        user.join("basic").join("script.xlc"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE library:libraries PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "libraries.dtd">
<library:libraries xmlns:library="http://openoffice.org/2000/library" xmlns:xlink="http://www.w3.org/1999/xlink">
 <library:library library:name="Convkit" xlink:href="$(USER)/basic/Convkit/script.xlb/" xlink:type="simple" library:link="false"/>
</library:libraries>
"#,
    )?;
    std::fs::write(
        library.join("script.xlb"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE library:library PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "library.dtd">
<library:library xmlns:library="http://openoffice.org/2000/library" library:name="Convkit" library:readonly="false" library:passwordprotected="false">
 <library:element library:name="Sheets"/>
</library:library>
"#,
    )?;
    std::fs::write(
        library.join("Sheets.xba"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE script:module PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "module.dtd">
<script:module xmlns:script="http://openoffice.org/2000/script" script:name="Sheets" script:language="StarBasic">{}</script:module>
"#,
            escape(SOURCE)
        ),
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What the macro did, as it wrote it down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// The PDF was written.
    pub exported: bool,
    /// Adjusting the layout failed, so the workbook was exported as it is.
    pub unfitted: bool,
    pub widened: u32,
    pub scaled: u32,
    /// The smallest scale a sheet was printed at, in percent.
    pub smallest: u32,
}

impl Report {
    /// The macro's report in `profile`, or `None` when it never ran.
    pub(crate) fn read(profile: &Path) -> Option<Report> {
        let text = std::fs::read_to_string(profile.join("user").join(REPORT)).ok()?;
        Some(Report::parse(&text))
    }

    fn parse(text: &str) -> Report {
        let mut r = Report::default();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let n = || value.parse().unwrap_or(0);
            match key {
                "status" => r.exported = value == "exported",
                "fit" => r.unfitted = value == "failed",
                "widened" => r.widened = n(),
                "scaled" => r.scaled = n(),
                "smallest" => r.smallest = n(),
                _ => {}
            }
        }
        r
    }

    /// What the notes say about an exported workbook. None when nothing
    /// changed: the workbook printed as it is set.
    pub(crate) fn notes(&self) -> Vec<String> {
        if !self.exported {
            return Vec::new();
        }
        if self.unfitted {
            return vec![UNFITTED_NOTE.to_string()];
        }
        let mut notes = Vec::new();
        match self.widened {
            0 => {}
            1 => notes.push("Widened 1 column whose numbers would otherwise print as ###.".into()),
            n => notes.push(format!(
                "Widened {n} columns whose numbers would otherwise print as ###."
            )),
        }
        match self.scaled {
            0 => {}
            1 => notes.push(format!(
                "Scaled 1 sheet down to fit the page width, to {}%.",
                self.smallest
            )),
            n => notes.push(format!(
                "Scaled {n} sheets down to fit the page width, the smallest to {}%.",
                self.smallest
            )),
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_names_both_paths_as_basic_strings() {
        assert_eq!(
            call("book.xlsx", "out/book.pdf"),
            r#"macro:///Convkit.Sheets.ToPdf("book.xlsx","out/book.pdf")"#
        );
        // Checked against LibreOffice 26.8: a comma, brackets, a space and
        // non-ASCII arrive as written; `%20` would arrive as a space.
        assert_eq!(
            call("a b,c(1) é.xlsx", r"C:\Users\x y\o.pdf"),
            r#"macro:///Convkit.Sheets.ToPdf("a b,c(1) é.xlsx","C:\Users\x y\o.pdf")"#
        );
        assert_eq!(
            call(r#"100% "q".xlsx"#, "o.pdf"),
            r#"macro:///Convkit.Sheets.ToPdf("100%25 ""q"".xlsx","o.pdf")"#
        );
    }

    #[test]
    fn the_paths_soffice_is_given_are_absolute() {
        let input = Path::new("in").join("book.xlsx");
        let output = Path::new(".convkit-1-0").join("book.pdf");
        let argv = vec![
            "--headless".to_string(),
            call(&input.to_string_lossy(), &output.to_string_lossy()),
        ];
        let run = with_absolute_paths(&argv, &input, &output).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(run[0], "--headless");
        assert_eq!(
            run[1],
            call(
                &cwd.join(&input).to_string_lossy(),
                &cwd.join(&output).to_string_lossy()
            )
        );
    }

    #[test]
    fn a_step_runs_the_macro_when_its_argv_calls_it() {
        let argv = |a: &str| vec!["--headless".to_string(), a.to_string()];
        assert!(is_call(&argv(&call("in.xlsx", "out.pdf"))));
        assert!(!is_call(&argv("--convert-to")));
    }

    #[test]
    fn the_seeded_profile_registers_the_library_and_keeps_its_list() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path()).unwrap();
        let user = dir.path().join("user");
        let read = |p: &Path| std::fs::read_to_string(user.join(p)).unwrap();
        assert!(read(Path::new("registrymodifications.xcu")).contains("ooSetupInstCompleted"));
        assert!(read(Path::new("basic/script.xlc")).contains(r#"library:name="Convkit""#));
        assert!(read(Path::new("basic/Convkit/script.xlb")).contains(r#"library:name="Sheets""#));
        let module = read(Path::new("basic/Convkit/Sheets.xba"));
        assert!(module.contains("Sub ToPdf(inPath As String, outPath As String)"));
        // The source is XML text: `&` and `"` escaped, so `"a" & b` survives.
        assert!(module.contains("&quot;status=exported&quot; &amp; Chr(10)"));
        assert!(!module.contains("\"status=exported\""));
    }

    #[test]
    fn the_report_reads_as_the_macro_writes_it() {
        let r = Report::parse("status=exported\r\nwidened=2\r\nscaled=1\r\nsmallest=52\r\n");
        assert_eq!(
            r,
            Report {
                exported: true,
                unfitted: false,
                widened: 2,
                scaled: 1,
                smallest: 52
            }
        );
        assert!(!Report::parse("status=unloaded\n").exported);
        assert!(Report::parse("status=exported\nfit=failed\n").unfitted);
    }

    #[test]
    fn the_notes_say_only_what_changed() {
        let notes = |text: &str| Report::parse(text).notes();
        assert!(notes("status=exported\nwidened=0\nscaled=0\nsmallest=100").is_empty());
        assert_eq!(
            notes("status=exported\nwidened=1\nscaled=0\nsmallest=100"),
            ["Widened 1 column whose numbers would otherwise print as ###."]
        );
        assert_eq!(
            notes("status=exported\nwidened=3\nscaled=1\nsmallest=52"),
            [
                "Widened 3 columns whose numbers would otherwise print as ###.",
                "Scaled 1 sheet down to fit the page width, to 52%."
            ]
        );
        assert_eq!(
            notes("status=exported\nwidened=0\nscaled=2\nsmallest=41"),
            ["Scaled 2 sheets down to fit the page width, the smallest to 41%."]
        );
        assert_eq!(notes("status=exported\nfit=failed"), [UNFITTED_NOTE]);
        assert!(notes("status=failed").is_empty());
    }
}
