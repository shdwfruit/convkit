//! `convkit-diff record`: runs every applicable conversion over a corpus
//! and writes a [`Baseline`].

use std::path::Path;

use convkit_core::{Backend, Resolver};

use crate::baseline::{entry_key, refs_dir_for, sanitize_for_filename, Baseline};
use crate::{convert, scan};

pub struct RecordSummary {
    pub recorded: usize,
    pub skipped: usize,
    pub failed: usize,
}

pub fn record(
    corpus_dir: &Path,
    baseline_path: &Path,
    resolver: &Resolver,
) -> Result<RecordSummary, String> {
    let scan = scan::scan_corpus(corpus_dir);

    let magick = resolver
        .resolve(Backend::Magick)
        .map_err(|e| format!("cannot resolve ImageMagick: {}", e.message))?
        .path;
    let ffmpeg = resolver.resolve(Backend::Ffmpeg).ok().map(|r| r.path);

    let refs_dir = refs_dir_for(baseline_path);
    std::fs::create_dir_all(&refs_dir)
        .map_err(|e| format!("cannot create {}: {e}", refs_dir.display()))?;

    let scratch = tempfile::tempdir().map_err(|e| format!("cannot create scratch dir: {e}"))?;

    let mut baseline = Baseline::new();
    baseline.context.backend_versions = convert::backend_versions(resolver);
    baseline.skipped = scan.skipped;

    let mut failed = 0usize;

    let backends = convert::Backends {
        resolver,
        magick: &magick,
        ffmpeg: ffmpeg.as_deref(),
    };

    for (i, planned) in scan.conversions.iter().enumerate() {
        let key = entry_key(&planned.input_rel, planned.to.ext());
        let unique_name = format!("{i:04}-{}", sanitize_for_filename(&key));

        match convert::convert_and_inspect(
            &backends,
            &planned.input_path,
            &planned.input_rel,
            planned.from,
            planned.to,
            scratch.path(),
            &unique_name,
        ) {
            Ok(fresh) => {
                let mut entry = fresh.entry;
                if crate::scope::is_magick_inspectable(planned.to) {
                    let ref_name = format!("{}.{}", sanitize_for_filename(&key), planned.to.ext());
                    let ref_path = refs_dir.join(&ref_name);
                    std::fs::copy(&fresh.output_path, &ref_path).map_err(|e| {
                        format!("cannot write reference {}: {e}", ref_path.display())
                    })?;
                    entry.pixel_ref = Some(ref_name);
                }
                baseline.entries.insert(key, entry);
            }
            Err(error) => {
                failed += 1;
                baseline.failed.push(crate::baseline::ConversionFailure {
                    input: planned.input_rel.clone(),
                    from: planned.from.ext().to_string(),
                    to: planned.to.ext().to_string(),
                    error,
                });
            }
        }
    }

    let recorded = baseline.entries.len();
    let skipped = baseline.skipped.len();
    baseline.save(baseline_path)?;

    Ok(RecordSummary {
        recorded,
        skipped,
        failed,
    })
}
