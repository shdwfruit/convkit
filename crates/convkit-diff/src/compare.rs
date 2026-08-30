//! `convkit-diff compare`: re-runs every conversion a [`Baseline`] recorded
//! and diffs the fresh result against it, axis by axis and separately --
//! "this separation is the point, because a change can be pixel-identical
//! and still drop a colour profile."

use std::collections::BTreeSet;
use std::path::Path;

use convkit_core::{Backend, Format, Resolver};

use crate::baseline::{refs_dir_for, Baseline, Entry};
use crate::report::{AxisDiff, CompareReport, EntryDiff, MissingInput, NewFailure, Tolerances};
use crate::{convert, inspect, scan};

pub fn compare(
    corpus_dir: &Path,
    baseline_path: &Path,
    resolver: &Resolver,
    tolerances: Tolerances,
) -> Result<CompareReport, String> {
    let baseline = Baseline::load(baseline_path)?;
    let refs_dir = refs_dir_for(baseline_path);

    let magick = resolver
        .resolve(Backend::Magick)
        .map_err(|e| format!("cannot resolve ImageMagick: {}", e.message))?
        .path;
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).ok().map(|r| r.path);

    let scratch = tempfile::tempdir().map_err(|e| format!("cannot create scratch dir: {e}"))?;

    let mut entry_diffs = Vec::new();
    let mut missing_inputs = Vec::new();
    let mut new_failures = Vec::new();
    let mut compared = 0usize;
    let mut total_regressions = 0usize;
    let mut entries_with_regressions = 0usize;

    let backends = convert::Backends {
        resolver,
        magick: &magick,
        ffmpeg: ffmpeg.as_deref(),
    };

    for (i, (key, expected)) in baseline.entries.iter().enumerate() {
        let input_path = corpus_dir.join(&expected.input);
        if !input_path.is_file() {
            missing_inputs.push(MissingInput {
                key: key.clone(),
                input: expected.input.clone(),
            });
            continue;
        }
        let Some(from) = Format::from_ext(&expected.from) else {
            return Err(format!(
                "baseline entry {key} has unrecognised from-format {}",
                expected.from
            ));
        };
        let Some(to) = Format::from_ext(&expected.to) else {
            return Err(format!(
                "baseline entry {key} has unrecognised to-format {}",
                expected.to
            ));
        };

        let unique_name = format!("{i:04}-{}", crate::baseline::sanitize_for_filename(key));
        let fresh = match convert::convert_and_inspect(
            &backends,
            &input_path,
            &expected.input,
            from,
            to,
            scratch.path(),
            &unique_name,
        ) {
            Ok(fresh) => fresh,
            Err(error) => {
                new_failures.push(NewFailure {
                    key: key.clone(),
                    input: expected.input.clone(),
                    from: expected.from.clone(),
                    to: expected.to.clone(),
                    error,
                });
                continue;
            }
        };
        compared += 1;

        let mut diffs = diff_entry(expected, &fresh.entry, &tolerances);

        if let Some(ref_name) = &expected.pixel_ref {
            let ref_path = refs_dir.join(ref_name);
            if fresh.entry.output_sha256 == expected.output_sha256 {
                // Bytes are identical -- RMSE is trivially 0, no need to
                // shell out to `magick compare` for it.
            } else if ref_path.is_file() {
                match inspect::compare_rmse(&magick, &fresh.output_path, &ref_path) {
                    Ok(rmse) => {
                        let regression = rmse > tolerances.pixel_rmse;
                        diffs.push(AxisDiff {
                            axis: "pixels".to_string(),
                            expected: "identical bytes (rmse 0)".to_string(),
                            actual: format!("bytes differ, rmse={rmse:.6}"),
                            regression,
                        });
                    }
                    Err(error) => diffs.push(AxisDiff {
                        axis: "pixels".to_string(),
                        expected: "comparable to the recorded reference".to_string(),
                        actual: format!("could not compare: {error}"),
                        regression: true,
                    }),
                }
            } else {
                diffs.push(AxisDiff {
                    axis: "pixels".to_string(),
                    expected: format!("reference file {ref_name} present"),
                    actual: "reference file missing from .refs directory".to_string(),
                    regression: true,
                });
            }
        }

        if !diffs.is_empty() {
            let regressions_here = diffs.iter().filter(|d| d.regression).count();
            total_regressions += regressions_here;
            if regressions_here > 0 {
                entries_with_regressions += 1;
            }
            entry_diffs.push(EntryDiff {
                key: key.clone(),
                input: expected.input.clone(),
                from: expected.from.clone(),
                to: expected.to.clone(),
                diffs,
            });
        }
    }

    let scan = scan::scan_corpus(corpus_dir);
    let baseline_keys: BTreeSet<&str> = baseline.entries.keys().map(String::as_str).collect();
    let mut new_conversions_available = Vec::new();
    for planned in &scan.conversions {
        let key = crate::baseline::entry_key(&planned.input_rel, planned.to.ext());
        if !baseline_keys.contains(key.as_str()) {
            new_conversions_available.push(key);
        }
    }
    new_conversions_available.sort();

    let current_versions = convert::backend_versions(resolver);
    let mut backend_version_changes = Vec::new();
    for (backend, current) in &current_versions {
        match baseline.context.backend_versions.get(backend) {
            Some(recorded) if recorded != current => {
                backend_version_changes.push(format!("{backend}: {recorded} -> {current}"));
            }
            None => backend_version_changes.push(format!("{backend}: (not recorded) -> {current}")),
            _ => {}
        }
    }

    Ok(CompareReport {
        tolerances,
        baseline_entries: baseline.entries.len(),
        compared,
        entries_with_regressions,
        total_regressions,
        entry_diffs,
        missing_inputs,
        new_failures,
        new_conversions_available,
        backend_version_changes,
        current_skip_count: scan.skipped.len(),
    })
}

fn diff_entry(expected: &Entry, actual: &Entry, tolerances: &Tolerances) -> Vec<AxisDiff> {
    let mut diffs = Vec::new();

    // --- Size: a headline number convkit publishes; a 5-10% regression
    // must fail rather than pass quietly. -------------------------------
    if expected.size_bytes != actual.size_bytes {
        let pct = if expected.size_bytes == 0 {
            if actual.size_bytes == 0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else {
            ((actual.size_bytes as f64 - expected.size_bytes as f64) / expected.size_bytes as f64)
                * 100.0
        };
        diffs.push(AxisDiff {
            axis: "size".to_string(),
            expected: format!("{} bytes", expected.size_bytes),
            actual: format!("{} bytes ({pct:+.2}%)", actual.size_bytes),
            regression: pct.abs() > tolerances.size_pct,
        });
    }

    // --- Dimensions and colourspace: cheap, and catches auto-orient
    // transposing an image. ----------------------------------------------
    if expected.dimensions != actual.dimensions {
        diffs.push(AxisDiff {
            axis: "dimensions".to_string(),
            expected: fmt_dims(expected.dimensions),
            actual: fmt_dims(actual.dimensions),
            regression: true,
        });
    }
    if expected.colorspace != actual.colorspace {
        diffs.push(AxisDiff {
            axis: "colorspace".to_string(),
            expected: fmt_opt(&expected.colorspace),
            actual: fmt_opt(&actual.colorspace),
            regression: true,
        });
    }

    // --- Metadata: orientation (EXIF for JPEG, native tag for TIFF) and
    // ICC profile presence/length/hash. Specific tags silently vanishing is
    // exactly the predicted failure mode -- "some metadata exists" is not
    // good enough, so these compare the tracked values exactly. ----------
    if expected.orientation != actual.orientation {
        diffs.push(AxisDiff {
            axis: "orientation".to_string(),
            expected: fmt_opt(&expected.orientation),
            actual: fmt_opt(&actual.orientation),
            regression: true,
        });
    }
    if expected.icc != actual.icc {
        diffs.push(AxisDiff {
            axis: "icc".to_string(),
            expected: fmt_icc(&expected.icc),
            actual: fmt_icc(&actual.icc),
            regression: true,
        });
    }

    // --- GIF: frame count dropping is a correctness regression; unique
    // colour count changing is not -- it's exactly what a future
    // imagequant swap is expected to improve. ----------------------------
    let expected_frames = expected.gif.as_ref().map(|g| g.frame_count);
    let actual_frames = actual.gif.as_ref().map(|g| g.frame_count);
    if expected_frames != actual_frames {
        diffs.push(AxisDiff {
            axis: "gif_frame_count".to_string(),
            expected: fmt_opt_display(expected_frames),
            actual: fmt_opt_display(actual_frames),
            regression: true,
        });
    }
    let expected_colors = expected.gif.as_ref().map(|g| g.unique_colors);
    let actual_colors = actual.gif.as_ref().map(|g| g.unique_colors);
    if expected_colors != actual_colors {
        diffs.push(AxisDiff {
            axis: "gif_unique_colors".to_string(),
            expected: fmt_opt_display(expected_colors),
            actual: fmt_opt_display(actual_colors),
            regression: false,
        });
    }

    diffs
}

fn fmt_dims(d: Option<(u32, u32)>) -> String {
    match d {
        Some((w, h)) => format!("{w}x{h}"),
        None => "(unknown)".to_string(),
    }
}

fn fmt_opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "(none)".to_string())
}

fn fmt_opt_display<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "(none)".to_string(),
    }
}

fn fmt_icc(icc: &Option<crate::baseline::IccInfo>) -> String {
    match icc {
        Some(i) => format!("{} bytes, sha256={}", i.len_bytes, &i.sha256[..12]),
        None => "(absent)".to_string(),
    }
}
