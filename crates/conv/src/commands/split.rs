//! `conv split IN [RANGE...]`: one PDF per page, or per page range.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::pdf::{self, PageRange};
use convkit_core::{ConvError, ErrorCode, Format, Remediation};
use serde_json::json;

use crate::cli::{Cli, SplitArgs};
use crate::commands::pdf_support;
use crate::input;
use crate::render;

pub fn run(cli: &Cli, args: &SplitArgs) -> i32 {
    let (input, ranges) = match check_args(args) {
        Ok(v) => v,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let subject = json!({ "input": input });
    let header = name_of(&input);
    // A missing qpdf (or a failed install) is a failure of this run, so it
    // goes in the result envelope like any other, as in `conv merge`.
    let qpdf = match pdf_support::resolve_qpdf(cli, !args.dry_run) {
        Ok(q) => q,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let plan = match pdf::plan_split(&qpdf.path, &input, &ranges, args.outdir.as_deref()) {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.dry_run {
        pdf_support::print_dry_run(cli, &plan);
        return 0;
    }

    if let Some(dir) = args.outdir.as_ref().filter(|_| !args.dry_run) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            let e = ConvError {
                code: ErrorCode::InvalidInvocation,
                message: format!("cannot create output directory {}: {e}", dir.display()),
                backend: None,
                remediation: Some(Remediation {
                    managed: None,
                    manual: Some(format!(
                        "create it yourself and check permissions, e.g. `mkdir -p {}`",
                        dir.display()
                    )),
                }),
            };
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    }

    let start = Instant::now();
    let mut on_event = pdf_support::verbose_printer(args.verbose);
    match pdf::run(&plan, &qpdf, args.overwrite, &mut on_event) {
        Ok(o) => {
            pdf_support::print_success(cli, subject, &o, start.elapsed());
            0
        }
        Err(e) => pdf_support::print_failure(cli, subject, &header, &e, false),
    }
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// A RANGE argument that is really another file, as when `conv split
/// *.pdf` matched several.
fn looks_like_a_file(arg: &str) -> bool {
    let p = Path::new(arg);
    pdf::parse_range(arg).is_err() && (Format::from_path(p) == Some(Format::Pdf) || p.is_file())
}

/// One PDF and its parsed ranges. The input's glob is expanded here
/// (Windows shells leave it to conv); more than one PDF is refused with the
/// count.
fn check_args(args: &SplitArgs) -> Result<(PathBuf, Vec<PageRange>), ConvError> {
    let expanded = input::expand_globs(std::slice::from_ref(&args.input), 1);
    let count = expanded.len() + args.ranges.iter().filter(|r| looks_like_a_file(r)).count();
    if count > 1 {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("split takes one PDF; got {count}. Run it once per file."),
        ));
    }
    let input = expanded
        .into_iter()
        .next()
        .unwrap_or_else(|| args.input.clone());
    if Format::from_path(&input) != Some(Format::Pdf) {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("split takes a PDF file; {} is not one", name_of(&input)),
        ));
    }
    let ranges = args
        .ranges
        .iter()
        .map(|r| pdf::parse_range(r))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((input, ranges))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &str, ranges: &[&str]) -> SplitArgs {
        SplitArgs {
            input: PathBuf::from(input),
            ranges: ranges.iter().map(|r| r.to_string()).collect(),
            outdir: None,
            overwrite: false,
            dry_run: false,
            verbose: false,
        }
    }

    #[test]
    fn ranges_are_parsed_in_order() {
        let (input, ranges) = check_args(&args("report.pdf", &["1-3", "11-z"])).unwrap();
        assert_eq!(input, PathBuf::from("report.pdf"));
        let texts: Vec<_> = ranges.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["1-3", "11-z"]);
    }

    #[test]
    fn a_valid_range_is_never_another_file() {
        assert!(!looks_like_a_file("5"));
        assert!(!looks_like_a_file("1-3"));
        assert!(!looks_like_a_file("z"));
        assert!(looks_like_a_file("b.pdf"));
        assert!(!looks_like_a_file("notes"));
    }

    #[test]
    fn more_than_one_pdf_is_refused_with_the_count() {
        let e = check_args(&args("a.pdf", &["b.pdf", "c.pdf"])).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "split takes one PDF; got 3. Run it once per file."
        );
    }

    #[test]
    fn a_non_pdf_and_a_bad_range_are_refused() {
        let e = check_args(&args("notes.docx", &[])).unwrap_err();
        assert_eq!(e.message, "split takes a PDF file; notes.docx is not one");
        let e = check_args(&args("report.pdf", &["3-"])).unwrap_err();
        assert!(
            e.message.starts_with("`3-` is not a page range"),
            "{}",
            e.message
        );
    }
}
