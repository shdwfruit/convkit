//! `conv split IN [RANGE...]`: one PDF per page, or per page range.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::pdf::{self, PageRange};
use convkit_core::{Backend, ConvError, ErrorCode, Format, Remediation};
use serde_json::json;

use crate::cli::{Cli, SplitArgs};
use crate::commands::pdf_support;
use crate::password::{self, Opener};
use crate::render;
use crate::{input, install_prompt};

pub fn run(cli: &Cli, args: &SplitArgs) -> i32 {
    let (input, ranges) = match check_args(args) {
        Ok(v) => v,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    let subject = json!({ "input": input.to_string_lossy() });
    let header = render::file_name_of(&input);
    // Check the input before offering to install qpdf.
    if !input.is_file() {
        let e = ConvError::new(
            ErrorCode::InputNotFound,
            format!("input not found: {}", input.display()),
        );
        return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run);
    }
    // A missing qpdf (or a failed install) is a failure of this run, so it
    // goes in the result envelope like any other, as in `conv merge`.
    let qpdf = match install_prompt::resolve_offering(cli, Backend::Qpdf, !args.dry_run) {
        Ok(q) => q,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let mut opener = match Opener::from_env() {
        Ok(o) => o,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let planned = pdf::plan_split(
        &qpdf.path,
        &input,
        &ranges,
        args.outdir.as_deref(),
        &mut |p, wrong| opener.next(p, wrong),
    )
    .and_then(|plan| pdf_support::locked(plan, &args.lock, "the split files", args.dry_run));
    let plan = match planned {
        Ok(p) => p,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    if args.dry_run {
        pdf_support::print_dry_run(cli, &plan);
        return 0;
    }

    if let Some(dir) = args.outdir.as_ref() {
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
            return pdf_support::print_failure(cli, subject, &header, &e, false);
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
            format!(
                "split takes a PDF file; {} is not one",
                render::file_name_of(&input)
            ),
        ));
    }
    if args.lock.password {
        if let Some(typed) = args
            .ranges
            .iter()
            .find(|r| pdf::parse_range(r).is_err() && !Path::new(r).exists())
        {
            return Err(password::typed_after_the_flag(typed));
        }
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
            lock: Default::default(),
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
    fn a_password_typed_after_the_flag_is_called_out() {
        let mut a = args("report.pdf", &["1-3", "hunter2"]);
        a.lock.password = true;
        let e = check_args(&a).unwrap_err();
        assert!(
            e.message.starts_with("--password takes no value"),
            "{}",
            e.message
        );
        assert!(e.message.contains("hunter2 is not a file"), "{}", e.message);
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
