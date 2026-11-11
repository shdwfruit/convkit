//! `conv pages IN [PAGES...] [OUT]`: keep, reorder, delete and turn a
//! PDF's pages into one file.

use std::path::{Path, PathBuf};
use std::time::Instant;

use convkit_core::pdf::{self, PageEdit};
use convkit_core::{Backend, ConvError, ErrorCode, Format, Remediation};
use serde_json::json;

use crate::cli::{Cli, PagesArgs};
use crate::commands::pdf_support;
use crate::password::{self, Opener};
use crate::render;
use crate::{input, install_prompt};

pub fn run(cli: &Cli, args: &PagesArgs) -> i32 {
    let (input, edit, output) = match check_args(args) {
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
    let qpdf = match install_prompt::resolve_offering(cli, Backend::Qpdf, !args.dry_run) {
        Ok(q) => q,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let mut opener = match Opener::from_env() {
        Ok(o) => o,
        Err(e) => return pdf_support::print_failure(cli, subject, &header, &e, args.dry_run),
    };
    let planned = pdf::plan_pages(
        &qpdf.path,
        &input,
        &edit,
        output.as_deref(),
        args.outdir.as_deref(),
        &mut |p, wrong| opener.next(p, wrong),
    )
    .and_then(|plan| {
        let what = render::file_name_of(&plan.outputs[0].path);
        pdf_support::locked(plan, &args.lock, &what, args.dry_run)
    });
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

/// How `--rotate` spells `turn`.
pub(crate) fn turn_word(turn: convkit_core::rotate::Turn) -> &'static str {
    match turn {
        convkit_core::rotate::Turn::Right => "right",
        convkit_core::rotate::Turn::Left => "left",
        convkit_core::rotate::Turn::Half => "180",
    }
}

/// `conv report.pdf --rotate right` asks for what `conv pages` does: say
/// so, rather than that `--rotate` does not apply to a PDF.
pub fn rotate_hint(cli: &Cli) -> Option<ConvError> {
    let turn = cli.rotate?;
    let [only] = cli.paths.as_slice() else {
        return None;
    };
    if Format::from_path(only) != Some(Format::Pdf) || cli.to.is_some() {
        return None;
    }
    let word = turn_word(turn);
    Some(ConvError {
        remediation: Some(Remediation {
            managed: None,
            manual: Some(format!("conv pages {} --rotate {word}", only.display())),
        }),
        ..invalid("--rotate turns images, video and GIFs; conv pages turns a PDF's pages".into())
    })
}

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// A PAGES argument that is really another file, as when `conv pages
/// *.pdf` matched several.
fn looks_like_a_file(arg: &str) -> bool {
    let p = Path::new(arg);
    pdf::parse_list(arg).is_err() && (Format::from_path(p) == Some(Format::Pdf) || p.is_file())
}

/// The PDF, what to do to its pages, and the file to write when the last
/// argument names one (under `-o`, by its name). The input's glob is
/// expanded here, as `conv split` does.
fn check_args(args: &PagesArgs) -> Result<(PathBuf, PageEdit, Option<PathBuf>), ConvError> {
    let expanded = input::expand_globs(std::slice::from_ref(&args.input), 1);
    let mut words: Vec<&str> = args.pages.iter().map(String::as_str).collect();
    // The last word names the output when it is a .pdf and not pages.
    let output = match words.last() {
        Some(last) if pdf::parse_list(last).is_err() && is_pdf_name(last) => {
            let out = words.pop().map(PathBuf::from);
            out.filter(|p| !input::is_extension_shorthand(p))
        }
        _ => None,
    };
    // `conv pages *.pdf --rotate right` with two PDFs reads as an input
    // and an output: the second, existing one is never written over, even
    // with -y.
    let taken = output.as_ref().filter(|o| words.is_empty() && o.is_file());
    let count = expanded.len()
        + words.iter().filter(|w| looks_like_a_file(w)).count()
        + usize::from(taken.is_some());
    if count > 1 {
        let mut message = format!("pages takes one PDF; got {count}. Run it once per file");
        if let Some(o) = taken {
            message.push_str(&format!(
                ", or remove {} to write the result there",
                o.display()
            ));
        }
        message.push('.');
        return Err(invalid(message));
    }
    let input = expanded
        .into_iter()
        .next()
        .unwrap_or_else(|| args.input.clone());
    if Format::from_path(&input) != Some(Format::Pdf) {
        return Err(invalid(format!(
            "pages takes a PDF file; {} is not one",
            render::file_name_of(&input)
        )));
    }
    if args.lock.password {
        if let Some(typed) = words
            .iter()
            .find(|w| pdf::parse_list(w).is_err() && !Path::new(w).exists())
        {
            return Err(password::typed_after_the_flag(typed));
        }
    }
    let mut edit = PageEdit::default();
    for w in &words {
        edit.keep.extend(pdf::parse_list(w)?);
    }
    for d in &args.delete {
        edit.delete.extend(pdf::parse_list(d)?);
    }
    for t in &args.rotate {
        edit.turns.push(pdf::pages::parse_turn(t)?);
    }
    if edit == PageEdit::default() {
        return Err(ConvError {
            remediation: Some(Remediation {
                managed: None,
                manual: Some(format!(
                    "e.g. `conv pages {} 1-3`, `--delete 2` or `--rotate right`",
                    args.input.display()
                )),
            }),
            ..invalid("say which pages to keep, delete or turn".to_string())
        });
    }
    let output = output
        .map(|o| input::in_outdir(&o, args.outdir.as_deref()))
        .transpose()?;
    Ok((input, edit, output))
}

/// `out.pdf`, `dir/out.pdf` or `.pdf`.
fn is_pdf_name(word: &str) -> bool {
    let p = Path::new(word);
    Format::from_path(p) == Some(Format::Pdf)
        || (input::is_extension_shorthand(p) && word.eq_ignore_ascii_case(".pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &str, pages: &[&str]) -> PagesArgs {
        PagesArgs {
            input: PathBuf::from(input),
            pages: pages.iter().map(|r| r.to_string()).collect(),
            delete: vec![],
            rotate: vec![],
            outdir: None,
            overwrite: false,
            dry_run: false,
            verbose: false,
            lock: Default::default(),
        }
    }

    fn keep_texts(edit: &PageEdit) -> Vec<&str> {
        edit.keep.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn pages_are_parsed_in_order_with_spaces_or_commas() {
        let (input, edit, output) = check_args(&args("report.pdf", &["3", "1-2,5-z"])).unwrap();
        assert_eq!(input, PathBuf::from("report.pdf"));
        assert_eq!(keep_texts(&edit), vec!["3", "1-2", "5-z"]);
        assert_eq!(output, None);
    }

    #[test]
    fn a_last_pdf_names_the_output_and_goes_under_outdir() {
        let (_, edit, output) = check_args(&args("report.pdf", &["2", "cover.pdf"])).unwrap();
        assert_eq!(keep_texts(&edit), vec!["2"]);
        assert_eq!(output, Some(PathBuf::from("cover.pdf")));
        let mut a = args("report.pdf", &["2", "cover.pdf"]);
        a.outdir = Some(PathBuf::from("out"));
        assert_eq!(
            check_args(&a).unwrap().2,
            Some(Path::new("out").join("cover.pdf"))
        );
        // `.pdf` is the derived name.
        assert_eq!(
            check_args(&args("report.pdf", &["2", ".pdf"])).unwrap().2,
            None
        );
    }

    #[test]
    fn delete_and_rotate_are_parsed() {
        let mut a = args("report.pdf", &[]);
        a.delete = vec!["4,6-7".into(), "9".into()];
        a.rotate = vec!["right:2,4".into(), "-90".into()];
        let (_, edit, _) = check_args(&a).unwrap();
        let deleted: Vec<_> = edit.delete.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(deleted, vec!["4", "6-7", "9"]);
        assert_eq!(edit.turns.len(), 2);
        assert_eq!(edit.turns[1], (convkit_core::rotate::Turn::Left, None));
        let mut a = args("report.pdf", &[]);
        a.rotate = vec!["sideways".into()];
        assert!(check_args(&a)
            .unwrap_err()
            .message
            .starts_with("invalid turn 'sideways'"));
    }

    #[test]
    fn nothing_asked_more_than_one_pdf_and_a_non_pdf_are_refused() {
        let e = check_args(&args("report.pdf", &[])).unwrap_err();
        assert_eq!(e.message, "say which pages to keep, delete or turn");
        let e = check_args(&args("a.pdf", &["b.pdf", "1", "c.pdf"])).unwrap_err();
        assert_eq!(
            e.message,
            "pages takes one PDF; got 2. Run it once per file."
        );
        let e = check_args(&args("notes.docx", &["1"])).unwrap_err();
        assert_eq!(e.message, "pages takes a PDF file; notes.docx is not one");
        let e = check_args(&args("report.pdf", &["3-"])).unwrap_err();
        assert!(
            e.message.starts_with("`3-` is not a page range"),
            "{}",
            e.message
        );
    }

    #[test]
    fn an_existing_pdf_as_the_only_word_is_a_second_input_not_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.pdf"), dir.path().join("b.pdf"));
        std::fs::write(&a, b"%PDF").unwrap();
        std::fs::write(&b, b"%PDF").unwrap();
        let mut glob = args(a.to_str().unwrap(), &[b.to_str().unwrap()]);
        glob.rotate = vec!["right".into()];
        glob.overwrite = true;
        let e = check_args(&glob).unwrap_err();
        assert_eq!(
            e.message,
            format!(
                "pages takes one PDF; got 2. Run it once per file, or remove {} to write \
                 the result there.",
                b.display()
            )
        );
        // With pages before it, an existing file is the output, as named.
        let named = args(a.to_str().unwrap(), &["1", b.to_str().unwrap()]);
        assert_eq!(check_args(&named).unwrap().2, Some(b));
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
    }
}
