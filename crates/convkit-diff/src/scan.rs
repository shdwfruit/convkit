//! Walks a corpus directory and expands it into the concrete list of
//! conversions to run -- shared by `record` and `compare` so they always
//! agree on what "every applicable conversion over the corpus" means.

use std::path::{Path, PathBuf};

use convkit_core::Format;

use crate::baseline::SkipRecord;
use crate::{corpus, scope};

pub struct PlannedConversion {
    pub input_path: PathBuf,
    pub input_rel: String,
    pub from: Format,
    pub to: Format,
}

pub struct ScanResult {
    pub conversions: Vec<PlannedConversion>,
    pub skipped: Vec<SkipRecord>,
}

/// Recognises every file under `root` (recursively), classifies it by
/// convkit-core's own `Format`, and expands each recognised, in-scope file
/// into one `PlannedConversion` per applicable target format. A file with
/// an extension `Format` doesn't recognise at all, or one recognised but
/// with no in-scope target (an audio/document file sitting in an otherwise
/// image-focused directory, for instance), is counted as a skip rather than
/// aborting the scan -- exactly the "skip unreadable or unsupported files
/// with a counted, reported skip" requirement, since a real photo library
/// is guaranteed to contain files this harness has no business touching.
pub fn scan_corpus(root: &Path) -> ScanResult {
    let mut conversions = Vec::new();
    let mut skipped = Vec::new();

    for file in corpus::walk_corpus(root) {
        let rel = corpus::relative_slash(root, &file);
        match Format::from_path(&file) {
            None => skipped.push(SkipRecord {
                input: rel,
                reason: "unrecognized extension".to_string(),
            }),
            Some(from) => {
                let targets = scope::applicable_targets(from);
                if targets.is_empty() {
                    skipped.push(SkipRecord {
                        input: rel,
                        reason: format!(
                            "recognised as {} but no in-scope conversion target",
                            from.ext()
                        ),
                    });
                } else {
                    for to in targets {
                        conversions.push(PlannedConversion {
                            input_path: file.clone(),
                            input_rel: rel.clone(),
                            from,
                            to,
                        });
                    }
                }
            }
        }
    }

    conversions.sort_by(|a, b| (&a.input_rel, a.to).cmp(&(&b.input_rel, b.to)));
    skipped.sort_by(|a, b| a.input.cmp(&b.input));

    ScanResult {
        conversions,
        skipped,
    }
}
