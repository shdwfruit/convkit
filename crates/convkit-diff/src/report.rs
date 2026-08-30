//! The shape of `compare`'s findings, and how they're printed -- human
//! ("which file, which conversion, which axis, expected versus actual") or
//! `--json`.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AxisDiff {
    pub axis: String,
    pub expected: String,
    pub actual: String,
    /// Whether this diff counts toward `compare`'s non-zero exit. Some axes
    /// (GIF unique-colour count) are deliberately *not* regressions on
    /// their own -- the whole point of the future imagequant swap this axis
    /// exists to gate is a *better* palette, i.e. a number that's supposed
    /// to change -- but are still surfaced so a human can see what moved.
    pub regression: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryDiff {
    pub key: String,
    pub input: String,
    pub from: String,
    pub to: String,
    pub diffs: Vec<AxisDiff>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingInput {
    pub key: String,
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewFailure {
    pub key: String,
    pub input: String,
    pub from: String,
    pub to: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Tolerances {
    pub size_pct: f64,
    pub pixel_rmse: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub tolerances: Tolerances,
    pub baseline_entries: usize,
    pub compared: usize,
    pub entries_with_regressions: usize,
    pub total_regressions: usize,
    pub entry_diffs: Vec<EntryDiff>,
    pub missing_inputs: Vec<MissingInput>,
    pub new_failures: Vec<NewFailure>,
    /// Conversions the corpus can produce now but the baseline never
    /// recorded -- e.g. a file was added to the corpus since `record` last
    /// ran. Informational only; never a regression, since nothing shrank.
    pub new_conversions_available: Vec<String>,
    pub backend_version_changes: Vec<String>,
    pub current_skip_count: usize,
}

impl CompareReport {
    pub fn has_regressions(&self) -> bool {
        self.total_regressions > 0
            || !self.missing_inputs.is_empty()
            || !self.new_failures.is_empty()
    }

    pub fn exit_code(&self) -> i32 {
        i32::from(self.has_regressions())
    }

    pub fn print_human(&self) {
        println!(
            "compared {} of {} baseline entries (size tolerance {:.2}%, pixel RMSE tolerance {:.4})",
            self.compared, self.baseline_entries, self.tolerances.size_pct, self.tolerances.pixel_rmse
        );
        if !self.backend_version_changes.is_empty() {
            println!("backend version changes since this baseline was recorded:");
            for change in &self.backend_version_changes {
                println!("  {change}");
            }
        }

        if self.entry_diffs.is_empty() {
            println!("no per-axis differences detected");
        } else {
            for entry in &self.entry_diffs {
                println!("{}  ({} -> {})", entry.input, entry.from, entry.to);
                for diff in &entry.diffs {
                    let tag = if diff.regression {
                        "REGRESSION"
                    } else {
                        "info"
                    };
                    println!(
                        "  [{tag}] {}: expected={} actual={}",
                        diff.axis, diff.expected, diff.actual
                    );
                }
            }
        }

        if !self.missing_inputs.is_empty() {
            println!("inputs the baseline recorded that are missing from this corpus:");
            for m in &self.missing_inputs {
                println!("  {}", m.input);
            }
        }

        if !self.new_failures.is_empty() {
            println!("conversions that succeeded when recorded but fail now:");
            for f in &self.new_failures {
                println!("  {}  ({} -> {}): {}", f.input, f.from, f.to, f.error);
            }
        }

        if !self.new_conversions_available.is_empty() {
            println!(
                "{} conversion(s) available in this corpus but not in the baseline (run `record` to add them):",
                self.new_conversions_available.len()
            );
            for c in &self.new_conversions_available {
                println!("  {c}");
            }
        }

        if self.current_skip_count > 0 {
            println!(
                "{} file(s) skipped (unrecognised or out of scope)",
                self.current_skip_count
            );
        }

        println!();
        if self.has_regressions() {
            println!(
                "RESULT: {} regression(s) across {} entrie(s); {} missing input(s); {} newly-failing conversion(s)",
                self.total_regressions,
                self.entries_with_regressions,
                self.missing_inputs.len(),
                self.new_failures.len()
            );
        } else {
            println!("RESULT: no regressions");
        }
    }

    pub fn print_json(&self) {
        match serde_json::to_string_pretty(self) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("failed to serialise report: {e}"),
        }
    }
}
