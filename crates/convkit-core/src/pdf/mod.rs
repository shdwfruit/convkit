//! PDF merge and split (`conv merge`, `conv split`), run on qpdf.
//!
//! qpdf rewrites a PDF's structure and never re-renders a page, so text,
//! links and images come through untouched. This module reads the inputs,
//! plans the qpdf calls together with the notes and warnings a person
//! should see, and runs them through a scratch folder so a failure leaves
//! nothing behind. Like the rest of convkit-core, it never prints.

pub mod plan;
pub mod range;
pub mod read;
pub mod run;

pub use plan::{plan_merge, plan_split, PdfJob, PdfPlan, PlannedOutput};
pub use range::{parse_range, PageRange};
pub use read::PdfInfo;
pub use run::{run, PdfOutcome, WrittenOutput};

use std::path::Path;

/// A path's file name for messages, or the whole path when it has none.
pub(crate) fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `p` as a qpdf argument. qpdf reads a leading `-` as an option and a
/// leading `@` as "read more arguments from this file", so a relative path
/// starting with either is passed as `./-name.pdf`.
pub(crate) fn path_arg(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if s.starts_with('-') || s.starts_with('@') {
        format!("./{s}")
    } else {
        s
    }
}

/// Why a qpdf call failed: its own `qpdf: ...` line without that prefix,
/// else its last line of output.
pub(crate) fn qpdf_reason(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(line) = lines.iter().find(|l| l.starts_with("qpdf: ")) {
        return line["qpdf: ".len()..].to_string();
    }
    lines
        .last()
        .map(|l| l.to_string())
        .unwrap_or_else(|| "qpdf exited with an error".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_path_qpdf_would_read_as_an_option_or_an_argument_file_gets_a_dot_slash() {
        assert_eq!(path_arg(Path::new("-cover.pdf")), "./-cover.pdf");
        assert_eq!(path_arg(Path::new("@list.pdf")), "./@list.pdf");
        assert_eq!(path_arg(Path::new("my report.pdf")), "my report.pdf");
        assert_eq!(path_arg(Path::new("dir/-x.pdf")), "dir/-x.pdf");
    }

    #[test]
    fn qpdf_reason_prefers_its_own_qpdf_line() {
        let stderr = "WARNING: fake.pdf: file is damaged\n\
                      qpdf: fake.pdf: unable to find trailer dictionary\n";
        assert_eq!(
            qpdf_reason(stderr),
            "fake.pdf: unable to find trailer dictionary"
        );
        assert_eq!(qpdf_reason("something odd\nlast line\n"), "last line");
        assert_eq!(qpdf_reason("   \n"), "qpdf exited with an error");
    }

    #[test]
    fn display_name_is_the_file_name() {
        assert_eq!(display_name(Path::new("/docs/report.pdf")), "report.pdf");
        assert_eq!(display_name(Path::new("report.pdf")), "report.pdf");
    }
}
