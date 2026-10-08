//! `conv merge IN... OUT`: join PDFs into one with qpdf.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::{pdf, ConvError, ErrorCode, Format, Kind, Remediation};
use serde_json::json;

use crate::cli::{Cli, MergeArgs};
use crate::commands::pdf_support;
use crate::input;
use crate::render;

pub fn run(cli: &Cli, args: &MergeArgs) -> i32 {
    let (inputs, output) = match split_paths(&args.paths) {
        Ok(p) => p,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let qpdf = match pdf_support::resolve_qpdf(cli, !args.dry_run) {
        Ok(q) => q,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let subject = json!({ "inputs": inputs, "output": output });
    let header = output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| output.display().to_string());

    let plan = match pdf::plan_merge(&qpdf.path, &inputs, &output) {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.dry_run {
        pdf_support::print_dry_run(cli, &plan);
        return 0;
    }

    let start = Instant::now();
    let mut on_event = pdf_support::verbose_printer(args.verbose);
    match pdf::run(&plan, &qpdf, args.overwrite, &mut on_event) {
        Ok(o) => {
            pdf_support::print_success(cli, subject, &o, start.elapsed());
            0
        }
        Err(mut e) => {
            if e.code == ErrorCode::OutputExists {
                e.remediation = Some(Remediation {
                    managed: None,
                    manual: Some(existing_output_fix(&args.paths)),
                });
            }
            pdf_support::print_failure(cli, subject, &header, &e, false)
        }
    }
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Splits the typed paths into inputs and the output, expanding globs
/// (never the output's) and folders, and checking every path is a PDF.
/// Whether the inputs exist is left to `pdf::plan_merge`.
fn split_paths(typed: &[PathBuf]) -> Result<(Vec<PathBuf>, PathBuf), ConvError> {
    let positionals = input::expand_globs(typed, typed.len().saturating_sub(1));
    let [raw_inputs @ .., output] = positionals.as_slice() else {
        return Err(invalid(
            "expected PDFs and then an output, e.g. `conv merge a.pdf b.pdf out.pdf`".into(),
        ));
    };
    if raw_inputs.is_empty() {
        return Err(invalid(
            "expected PDFs and then an output, e.g. `conv merge a.pdf b.pdf out.pdf`".into(),
        ));
    }
    if output.is_dir() {
        return Err(invalid(format!(
            "{} is a folder; name the merged file, e.g. {}",
            output.display(),
            output.join("merged.pdf").display()
        )));
    }
    if Format::from_path(output) != Some(Format::Pdf) {
        return Err(invalid(format!(
            "the merged file must be a .pdf, got {}",
            name_of(output)
        )));
    }

    let mut inputs = Vec::new();
    for p in raw_inputs {
        if p.is_dir() {
            let found = input::pdfs_in(p, output)?;
            if found.is_empty() {
                return Err(invalid(format!("{} has no PDF files", p.display())));
            }
            inputs.extend(found);
            continue;
        }
        match Format::from_path(p) {
            Some(Format::Pdf) => inputs.push(p.clone()),
            other => {
                let mut e = invalid(format!("merge takes PDF files; {} is not one", name_of(p)));
                if other.is_some_and(|f| f.kind() == Kind::Image) {
                    e.remediation = Some(Remediation {
                        managed: None,
                        manual: Some(
                            "to turn images into one PDF: conv a.png b.png out.pdf".into(),
                        ),
                    });
                }
                return Err(e);
            }
        }
    }
    Ok((inputs, output.clone()))
}

/// The `try` line when the output already exists. The usual cause is a
/// glob that ended in an existing PDF (`conv merge *.pdf`), so it suggests
/// the same command with `merged.pdf` as the output. `-y` is already named
/// in the message itself.
fn existing_output_fix(typed: &[PathBuf]) -> String {
    let shown: Vec<String> = typed.iter().map(|p| p.display().to_string()).collect();
    let last = shown.last().cloned().unwrap_or_default();
    let listed = if shown.len() > 4 {
        format!("{} ... {last}", shown[0])
    } else {
        shown.join(" ")
    };
    format!("conv merge {listed} merged.pdf (if {last} was meant as an input)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn the_last_path_is_the_output() {
        let (inputs, output) = split_paths(&p(&["a.pdf", "b.pdf", "out.pdf"])).unwrap();
        assert_eq!(inputs, p(&["a.pdf", "b.pdf"]));
        assert_eq!(output, PathBuf::from("out.pdf"));
    }

    #[test]
    fn shape_errors_say_what_to_type() {
        let e = split_paths(&p(&["a.pdf"])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "expected PDFs and then an output, e.g. `conv merge a.pdf b.pdf out.pdf`"
        );
        let e = split_paths(&p(&["a.pdf", "b.pdf", "out.docx"])).unwrap_err();
        assert_eq!(e.message, "the merged file must be a .pdf, got out.docx");
        let e = split_paths(&p(&["a.png", "b.pdf", "out.pdf"])).unwrap_err();
        assert_eq!(e.message, "merge takes PDF files; a.png is not one");
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "to turn images into one PDF: conv a.png b.png out.pdf"
        );
        let e = split_paths(&p(&["notes.docx", "out.pdf"])).unwrap_err();
        assert!(
            e.remediation.is_none(),
            "only images get the image-merge hint"
        );
    }

    #[test]
    fn a_folder_adds_its_pdfs_and_an_empty_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let scans = dir.path().join("scans");
        std::fs::create_dir(&scans).unwrap();
        let e = split_paths(&[scans.clone(), dir.path().join("out.pdf")]).unwrap_err();
        assert_eq!(e.message, format!("{} has no PDF files", scans.display()));
        for name in ["p2.pdf", "p10.pdf", "all.pdf"] {
            std::fs::write(scans.join(name), b"x").unwrap();
        }
        let (inputs, _) = split_paths(&[scans.clone(), scans.join("all.pdf")]).unwrap();
        assert_eq!(inputs, vec![scans.join("p2.pdf"), scans.join("p10.pdf")]);
    }

    #[test]
    fn the_existing_output_fix_suggests_a_new_name() {
        assert_eq!(
            existing_output_fix(&p(&["a.pdf", "b.pdf", "report.pdf"])),
            "conv merge a.pdf b.pdf report.pdf merged.pdf (if report.pdf was meant as an input)"
        );
        assert_eq!(
            existing_output_fix(&p(&["a.pdf", "b.pdf", "c.pdf", "d.pdf", "report.pdf"])),
            "conv merge a.pdf ... report.pdf merged.pdf (if report.pdf was meant as an input)"
        );
    }
}
