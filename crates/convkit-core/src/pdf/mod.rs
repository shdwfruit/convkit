//! PDF merge, split, page edits and passwords (`conv merge`, `conv
//! split`, `conv pages`, `--password`, `--unlock`), run on qpdf.
//!
//! qpdf rewrites a PDF's structure and never re-renders a page, so text,
//! links and images come through untouched. This module reads the inputs,
//! plans the qpdf calls together with the notes and warnings a person
//! should see, and runs them through a scratch folder so a failure leaves
//! nothing behind. Like the rest of convkit-core, it never prints, and it
//! never asks for a password itself: the caller passes `AskPassword`.

pub mod lock;
pub mod pages;
pub mod plan;
pub mod range;
pub mod read;
pub mod run;

pub use lock::{Lock, Password, Restriction, PASSWORD_ENV};
pub use pages::{plan_pages, PageEdit};
pub use plan::{plan_merge, plan_rewrite, plan_split, PdfJob, PdfPlan, PlannedOutput, Rewrite};
pub use range::{parse_list, parse_range, PageRange};
pub use read::{open, AskPassword, PdfInfo};
pub use run::{run, PdfOutcome, WrittenOutput};

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use crate::procutil::backend_command;

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

/// Runs qpdf with `argv`. With `secret` set (some argument holds a
/// password) the arguments go to qpdf on its standard input, as `qpdf @-`
/// reads them, one per line, so no password is ever on a command line
/// another user could read in the process list. `cwd` is where it runs.
pub(crate) fn qpdf_output(
    qpdf: &Path,
    argv: &[String],
    secret: bool,
    cwd: Option<&Path>,
) -> std::io::Result<Output> {
    let mut cmd = backend_command(qpdf);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if !secret {
        return cmd.args(argv).output();
    }
    if argv.iter().any(|a| a.contains(['\n', '\r'])) {
        return Err(std::io::Error::other(
            "a file name with a line break can't be passed to qpdf with a password",
        ));
    }
    let mut child = cmd
        .arg("@-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut lines = argv.join("\n");
    lines.push('\n');
    // qpdf reads every argument before it writes anything, but a thread
    // keeps a full pipe from ever deadlocking the two of us.
    let mut stdin = child.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || stdin.write_all(lines.as_bytes()));
    let out = child.wait_with_output()?;
    // A qpdf that stopped early closed its end; its exit status and stderr
    // say why, which a broken pipe here would only hide.
    let _ = writer.join();
    Ok(out)
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
