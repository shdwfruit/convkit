//! convkit-diff: the differential conversion harness.
//!
//! Substantiates convkit's "measurably better defaults" claim with a
//! repeatable measurement, and gates every future engine swap (typst as a
//! library, comrak+docx-rs instead of pandoc, imagequant instead of
//! ffmpeg's palettegen) so a change never ships unless this harness says it
//! did not regress.
//!
//! Two subcommands: `record` runs every applicable conversion over a
//! corpus and writes a baseline; `compare` re-runs and diffs against it,
//! per axis, separately -- because a change can be pixel-identical and
//! still drop a colour profile. `gen-corpus` synthesises the adversarial
//! corpus described in the harness's design report.
//!
//! A new workspace member, `publish = false`: a development tool, not part
//! of what convkit ships. It never touches convkit-core's or conv's own
//! behaviour -- it consumes convkit-core exactly as any external caller
//! would, through `Resolver`, `build_plan`, and `exec::run`.

mod backend_paths;
mod baseline;
mod compare;
mod convert;
mod corpus;
mod gif_colors;
mod inspect;
mod record;
mod report;
mod scan;
mod scope;
mod synth;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use backend_paths::BackendPaths;
use report::Tolerances;

#[derive(Parser, Debug)]
#[command(
    name = "convkit-diff",
    version,
    about = "Differential conversion harness for convkit: substantiates the quality claim, gates every engine swap"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    backends: BackendPaths,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate the synthetic adversarial corpus (all EXIF orientations, a
    /// non-sRGB ICC profile, progressive/CMYK JPEG, palette/16-bit/alpha
    /// PNG, palette/grayscale TIFF, a transparent SVG, a multi-frame GIF)
    /// plus the real fixtures already in the repo, into `out_dir`.
    GenCorpus { out_dir: PathBuf },

    /// Run every applicable conversion over `corpus_dir` and write a
    /// baseline to `baseline_json`.
    Record {
        corpus_dir: PathBuf,
        baseline_json: PathBuf,
    },

    /// Re-run every conversion `baseline_json` recorded over `corpus_dir`
    /// and report per-axis regressions. Exits non-zero on any regression.
    Compare {
        corpus_dir: PathBuf,
        baseline_json: PathBuf,
        /// Emit a machine-readable JSON report instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Allowed output-size drift before it counts as a regression, as a
        /// percentage of the recorded size.
        #[arg(long, default_value_t = 1.0)]
        size_tolerance_pct: f64,
        /// Allowed normalised (0..1) RMSE between a fresh output and its
        /// recorded reference before it counts as a pixel regression.
        #[arg(long, default_value_t = 0.0005)]
        pixel_rmse_tolerance: f64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let resolver = cli.backends.resolver();

    match cli.command {
        Command::GenCorpus { out_dir } => match corpus::gen_corpus(&out_dir, &resolver) {
            Ok(report) => {
                println!(
                    "wrote {} file(s) to {}:",
                    report.written.len(),
                    out_dir.display()
                );
                for path in &report.written {
                    println!("  {}", path.display());
                }
                for note in &report.notes {
                    println!("note: {note}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gen-corpus failed: {e}");
                ExitCode::FAILURE
            }
        },

        Command::Record {
            corpus_dir,
            baseline_json,
        } => match record::record(&corpus_dir, &baseline_json, &resolver) {
            Ok(summary) => {
                println!(
                    "recorded {} conversion(s), skipped {} file(s), {} conversion(s) failed",
                    summary.recorded, summary.skipped, summary.failed
                );
                println!("baseline written to {}", baseline_json.display());
                if summary.failed > 0 {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("record failed: {e}");
                ExitCode::FAILURE
            }
        },

        Command::Compare {
            corpus_dir,
            baseline_json,
            json,
            size_tolerance_pct,
            pixel_rmse_tolerance,
        } => {
            let tolerances = Tolerances {
                size_pct: size_tolerance_pct,
                pixel_rmse: pixel_rmse_tolerance,
            };
            match compare::compare(&corpus_dir, &baseline_json, &resolver, tolerances) {
                Ok(report) => {
                    if json {
                        report.print_json();
                    } else {
                        report.print_human();
                    }
                    ExitCode::from(report.exit_code() as u8)
                }
                Err(e) => {
                    eprintln!("compare failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
