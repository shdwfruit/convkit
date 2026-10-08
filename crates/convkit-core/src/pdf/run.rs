//! Running a `PdfPlan`: every qpdf call writes into one scratch folder, and
//! the outputs move into place only once all of them exist.

use std::path::{Path, PathBuf};

use super::display_name;
use super::plan::{PdfJob, PdfPlan};
use super::range::join_and;
use crate::error::{ConvError, ErrorCode, Result};
use crate::exec::{self, BackendOutput, Event};
use crate::procutil::backend_command;
use crate::resolve::ResolvedBackend;
use crate::{winpath, Backend};

/// One file a run wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenOutput {
    pub path: PathBuf,
    pub pages: Vec<u32>,
    pub bytes: u64,
}

/// A finished merge or split.
#[derive(Debug, Clone)]
pub struct PdfOutcome {
    pub merge: bool,
    pub outputs: Vec<WrittenOutput>,
    /// Printed as `note` lines; JSON `warnings`.
    pub notes: Vec<String>,
    /// Printed as `warning` lines on stderr; JSON `notes`.
    pub warnings: Vec<String>,
    pub backend_output: Vec<BackendOutput>,
    /// qpdf's version, for JSON `backends`.
    pub version: String,
}

const REPORT_CAP: usize = 16 * 1024;

/// Runs `plan` with `qpdf`. Refuses before running anything when an output
/// exists (unless `overwrite`) or names a reserved Windows device. Every
/// call writes into one scratch folder beside the outputs, which is removed
/// on every exit path; outputs move into place only after all calls have
/// succeeded and every output is non-empty.
pub fn run(
    plan: &PdfPlan,
    qpdf: &ResolvedBackend,
    overwrite: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<PdfOutcome> {
    for o in &plan.outputs {
        winpath::check_output_name(&o.path)?;
        if o.path.is_dir() {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} is a folder", o.path.display()),
            ));
        }
        if o.path.exists() && !overwrite {
            return Err(ConvError::new(
                ErrorCode::OutputExists,
                format!("{} exists; pass -y to overwrite", o.path.display()),
            ));
        }
    }
    let dest_dir = plan
        .outputs
        .first()
        .and_then(|o| o.path.parent())
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = exec::make_scratch_dir(dest_dir)?;
    let _guard = exec::ScratchGuard::new(scratch.clone());

    let commands = plan.commands_into(Some(&scratch));
    let total = commands.len();
    let mut backend_output = Vec::new();
    let mut qpdf_warned = false;
    for (index, argv) in commands.iter().enumerate() {
        on_event(Event::StepStarted {
            index,
            total,
            backend: Backend::Qpdf,
        });
        let mut cmd = backend_command(&qpdf.path);
        cmd.args(argv);
        on_event(Event::StepSpawned {
            index,
            program: qpdf.path.clone(),
            argv: argv.clone(),
        });
        let out = cmd.output().map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", qpdf.path.display()),
            )
        })?;
        let report = String::from_utf8_lossy(&out.stderr).into_owned();
        if !report.trim().is_empty() {
            let tail = exec::tail_str(&report, REPORT_CAP).to_string();
            on_event(Event::StepReport {
                index,
                backend: Backend::Qpdf,
                report: tail.clone(),
            });
            backend_output.push(BackendOutput {
                backend: Backend::Qpdf,
                stderr: tail,
            });
        }
        match out.status.code() {
            Some(0) => {}
            Some(3) => qpdf_warned = true,
            _ => {
                return Err(ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("qpdf failed: {}", super::qpdf_reason(&report)),
                ));
            }
        }
        on_event(Event::StepFinished { index });
    }

    let mut produced = Vec::with_capacity(plan.outputs.len());
    for (i, planned) in plan.outputs.iter().enumerate() {
        let path = plan.scratch_output(&scratch, i);
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            return Err(ConvError::new(
                ErrorCode::ConversionFailed,
                format!(
                    "qpdf produced no output for {}",
                    display_name(&planned.path)
                ),
            ));
        }
        produced.push((path, bytes));
    }

    let moves: Vec<(PathBuf, PathBuf)> = produced
        .iter()
        .zip(&plan.outputs)
        .map(|((from, _), planned)| (from.clone(), planned.path.clone()))
        .collect();
    place_outputs(&moves, &mut |from, to| std::fs::rename(from, to))?;
    let outputs: Vec<WrittenOutput> = produced
        .into_iter()
        .zip(&plan.outputs)
        .map(|((_, bytes), planned)| WrittenOutput {
            path: planned.path.clone(),
            pages: planned.pages.clone(),
            bytes,
        })
        .collect();

    let mut warnings = plan.warnings.clone();
    if qpdf_warned && !plan.repaired {
        warnings.push(match &plan.job {
            PdfJob::Merge { output, .. } => format!(
                "qpdf reported problems while writing {}; check it.",
                display_name(output)
            ),
            _ => "qpdf reported problems while writing the split files; check them.".to_string(),
        });
    }
    Ok(PdfOutcome {
        merge: plan.is_merge(),
        outputs,
        notes: plan.notes.clone(),
        warnings,
        backend_output,
        version: qpdf.version.clone(),
    })
}

/// Moves each `(produced, target)` pair into place. On the first failure it
/// removes the targets this call created, so a failed move leaves no mixed
/// set. Files replaced under `-y` are not restored.
fn place_outputs(
    moves: &[(PathBuf, PathBuf)],
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let existed: Vec<bool> = moves.iter().map(|(_, to)| to.exists()).collect();
    for (i, (from, to)) in moves.iter().enumerate() {
        if let Err(e) = rename(from, to) {
            let mut replaced = Vec::new();
            for (j, (_, placed)) in moves[..i].iter().enumerate() {
                if existed[j] {
                    replaced.push(display_name(placed));
                } else {
                    let _ = std::fs::remove_file(placed);
                }
            }
            let tail = if replaced.is_empty() {
                "nothing was written".to_string()
            } else {
                format!("{} had already been replaced", join_and(&replaced))
            };
            return Err(ConvError::new(
                ErrorCode::ConversionFailed,
                format!(
                    "could not move {} into place: {e}; {tail}",
                    display_name(to)
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::parse_range;
    use crate::pdf::plan::{merge_plan, split_plan};
    use crate::pdf::PdfInfo;
    use crate::Source;

    fn info(path: &Path, pages: u32) -> PdfInfo {
        PdfInfo {
            path: path.to_path_buf(),
            pages,
            has_bookmarks: false,
            restricted: false,
            damaged: false,
        }
    }

    fn missing_qpdf() -> ResolvedBackend {
        ResolvedBackend {
            backend: Backend::Qpdf,
            path: PathBuf::from("/nonexistent/qpdf"),
            version: "0".into(),
            source: Source::Override,
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_existing_output_is_refused_before_qpdf_runs() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.pdf");
        std::fs::write(&out, b"keep me").unwrap();
        let plan = merge_plan(&[info(&dir.path().join("a.pdf"), 1)], &out).unwrap();

        let e = run(&plan, &missing_qpdf(), false, &mut |_| {}).unwrap_err();

        assert_eq!(e.code, ErrorCode::OutputExists);
        assert_eq!(
            e.message,
            format!("{} exists; pass -y to overwrite", out.display())
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"keep me");
        assert_eq!(entries(dir.path()), vec!["out.pdf"], "no scratch folder");
    }

    #[test]
    fn a_missing_output_folder_is_refused_before_qpdf_runs() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("missing").join("out.pdf");
        let plan = merge_plan(&[info(&dir.path().join("a.pdf"), 1)], &out).unwrap();

        let e = run(&plan, &missing_qpdf(), false, &mut |_| {}).unwrap_err();

        assert!(
            e.message.contains("output directory does not exist"),
            "{}",
            e.message
        );
        assert!(entries(dir.path()).is_empty());
    }

    #[cfg(unix)]
    fn stub_qpdf(body: &str) -> (tempfile::TempDir, ResolvedBackend) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qpdf-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = ResolvedBackend {
            backend: Backend::Qpdf,
            path,
            version: "stub".into(),
            source: Source::Override,
        };
        (dir, resolved)
    }

    /// Writes `%PDF-stub` to its last argument, the way qpdf writes an
    /// output; fails instead when that argument ends in `fail_on`.
    #[cfg(unix)]
    fn writing_stub(fail_on: &str) -> (tempfile::TempDir, ResolvedBackend) {
        stub_qpdf(&format!(
            "for last in \"$@\"; do :; done\n\
             case \"$last\" in *{fail_on}) echo \"qpdf: $last: boom\" >&2; exit 2;; esac\n\
             printf '%%PDF-stub' > \"$last\""
        ))
    }

    #[cfg(unix)]
    #[test]
    fn every_output_moves_into_place_once_all_calls_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("report.pdf");
        std::fs::write(&input, b"%PDF").unwrap();
        let ranges = [parse_range("1-3").unwrap(), parse_range("4").unwrap()];
        let plan = split_plan(&info(&input, 4), &ranges, dir.path()).unwrap();
        let (_stub_dir, qpdf) = writing_stub("never");
        let mut spawned = 0;

        let o = run(&plan, &qpdf, false, &mut |e| {
            if matches!(e, Event::StepSpawned { .. }) {
                spawned += 1;
            }
        })
        .unwrap();

        assert_eq!(spawned, 2);
        assert!(!o.merge);
        assert_eq!(o.outputs.len(), 2);
        assert_eq!(o.outputs[0].pages, vec![1, 2, 3]);
        assert_eq!(o.outputs[0].bytes, 9);
        assert_eq!(o.version, "stub");
        assert_eq!(
            entries(dir.path()),
            vec!["report-1-3.pdf", "report-4.pdf", "report.pdf"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_call_leaves_no_output_and_no_scratch_behind() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("report.pdf");
        std::fs::write(&input, b"%PDF").unwrap();
        let ranges = [parse_range("1-3").unwrap(), parse_range("4").unwrap()];
        let plan = split_plan(&info(&input, 4), &ranges, dir.path()).unwrap();
        let (_stub_dir, qpdf) = writing_stub("r-2.pdf");

        let e = run(&plan, &qpdf, false, &mut |_| {}).unwrap_err();

        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert!(e.message.starts_with("qpdf failed: "), "{}", e.message);
        assert!(e.message.ends_with("r-2.pdf: boom"), "{}", e.message);
        assert_eq!(entries(dir.path()), vec!["report.pdf"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_call_that_writes_nothing_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.pdf");
        std::fs::write(&input, b"%PDF").unwrap();
        let plan = merge_plan(&[info(&input, 1)], &dir.path().join("out.pdf")).unwrap();
        let (_stub_dir, qpdf) = stub_qpdf("exit 0");

        let e = run(&plan, &qpdf, false, &mut |_| {}).unwrap_err();

        assert_eq!(e.message, "qpdf produced no output for out.pdf");
        assert_eq!(entries(dir.path()), vec!["a.pdf"]);
    }

    #[cfg(unix)]
    #[test]
    fn qpdfs_warnings_exit_adds_a_warning_unless_a_repair_was_already_reported() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("a.pdf");
        std::fs::write(&input, b"%PDF").unwrap();
        let plan = merge_plan(&[info(&input, 1)], &dir.path().join("out.pdf")).unwrap();
        let (_stub_dir, qpdf) = stub_qpdf(
            "for last in \"$@\"; do :; done\n\
             printf '%%PDF-stub' > \"$last\"\n\
             echo 'WARNING: a.pdf: something odd' >&2\nexit 3",
        );

        let o = run(&plan, &qpdf, false, &mut |_| {}).unwrap();

        assert_eq!(
            o.warnings,
            vec!["qpdf reported problems while writing out.pdf; check it."]
        );
        assert_eq!(o.backend_output.len(), 1);
        assert!(o.backend_output[0].stderr.contains("something odd"));
    }

    #[test]
    fn a_folder_output_is_refused_even_with_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.pdf");
        std::fs::create_dir(&out).unwrap();
        let plan = merge_plan(&[info(&dir.path().join("a.pdf"), 1)], &out).unwrap();

        let e = run(&plan, &missing_qpdf(), true, &mut |_| {}).unwrap_err();

        assert_eq!(e.code, ErrorCode::OutputExists);
        assert_eq!(e.message, format!("{} is a folder", out.display()));
        assert!(out.is_dir());
        assert_eq!(entries(dir.path()), vec!["out.pdf"]);
    }

    fn failing_second(calls: &mut u32) -> impl FnMut(&Path, &Path) -> std::io::Result<()> + '_ {
        move |from, to| {
            *calls += 1;
            if *calls == 2 {
                Err(std::io::Error::other("locked"))
            } else {
                std::fs::rename(from, to)
            }
        }
    }

    fn staged(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        (1..=2)
            .map(|n| {
                let from = dir.join(format!("new{n}.tmp"));
                std::fs::write(&from, format!("new{n}")).unwrap();
                (from, dir.join(format!("out{n}.pdf")))
            })
            .collect()
    }

    #[test]
    fn a_failed_move_removes_what_it_placed() {
        let dir = tempfile::tempdir().unwrap();
        let moves = staged(dir.path());
        let mut calls = 0;

        let e = place_outputs(&moves, &mut failing_second(&mut calls)).unwrap_err();

        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert!(
            e.message
                .starts_with("could not move out2.pdf into place: "),
            "{}",
            e.message
        );
        assert!(
            e.message.ends_with("; nothing was written"),
            "{}",
            e.message
        );
        assert!(!moves[0].1.exists());
        assert!(!moves[1].1.exists());
    }

    #[test]
    fn a_failed_move_keeps_and_names_a_file_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let moves = staged(dir.path());
        std::fs::write(&moves[0].1, b"old").unwrap();
        let mut calls = 0;

        let e = place_outputs(&moves, &mut failing_second(&mut calls)).unwrap_err();

        assert!(
            e.message.ends_with("; out1.pdf had already been replaced"),
            "{}",
            e.message
        );
        assert_eq!(std::fs::read(&moves[0].1).unwrap(), b"new1");
    }

    #[test]
    fn place_outputs_moves_everything_when_all_renames_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let moves = staged(dir.path());

        place_outputs(&moves, &mut |from, to| std::fs::rename(from, to)).unwrap();

        assert_eq!(std::fs::read(&moves[0].1).unwrap(), b"new1");
        assert_eq!(std::fs::read(&moves[1].1).unwrap(), b"new2");
    }
}
