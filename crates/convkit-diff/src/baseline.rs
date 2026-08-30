//! The on-disk baseline format: what `record` writes and `compare` reads
//! back. Deliberately plain, sorted-key JSON (`BTreeMap`, pretty-printed) so
//! a baseline change is reviewable in a pull request diff -- a regression
//! that flips one field (an ICC hash, a byte count) should show up as a
//! one-line diff, not a reshuffled blob.
//!
//! The full reference *pixels* a `compare` run diffs against are deliberately
//! NOT embedded in this JSON file -- that would make it unreadable as a
//! diff. They live as ordinary files in a sibling `<baseline>.refs/`
//! directory instead; `Entry::pixel_ref` names the one for its conversion,
//! relative to that directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped whenever the shape of [`Baseline`] changes incompatibly, so
/// `compare` can give a clear error instead of a confusing serde failure
/// when pointed at a baseline written by an older/newer version of this
/// tool.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Context {
    /// Resolved backend versions at the time this baseline was produced --
    /// e.g. `"magick": "7.1.2-29"`. Informational: a version bump here
    /// explains a whole-baseline diff without anyone having to go dig up
    /// what changed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backend_versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IccInfo {
    pub len_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GifInfo {
    pub frame_count: u32,
    pub unique_colors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Corpus-relative input path, forward-slash normalised so the baseline
    /// is identical whether it was recorded on Windows or elsewhere.
    pub input: String,
    pub from: String,
    pub to: String,
    /// `"<exe> <version>"` for every backend the conversion actually ran
    /// through, in step order.
    pub backends: Vec<String>,

    pub size_bytes: u64,
    /// SHA-256 of the raw output file bytes. A cheap, oracle-independent
    /// "did anything at all change" signal; when this matches, the pixel
    /// axis is `compare`'s free win -- RMSE is trivially 0 without ever
    /// shelling out to `magick compare`.
    pub output_sha256: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colorspace: Option<String>,
    /// ImageMagick's `%[orientation]` string (`TopLeft`, `RightTop`, ...),
    /// which is format-agnostic: for JPEG it reads the embedded EXIF
    /// `Orientation` tag, for TIFF the native TIFF orientation tag. `None`
    /// only when the backend genuinely couldn't be resolved to inspect the
    /// output; an image with no orientation tag at all still gets the
    /// literal string `"Undefined"`, which is itself a meaningful, tracked
    /// value (a converted output not carrying an orientation tag at all is
    /// different from one that carries `"TopLeft"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icc: Option<IccInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gif: Option<GifInfo>,

    /// Path to the stored reference output, relative to the baseline's
    /// `.refs/` directory. `None` for a conversion whose target format
    /// isn't itself pixel-inspectable by ImageMagick (namely, the
    /// gif->video pathway's `.mp4` output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipRecord {
    pub input: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionFailure {
    pub input: String,
    pub from: String,
    pub to: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    #[serde(default)]
    pub context: Context,
    /// Keyed by `"<input> -> <to>"`, sorted (`BTreeMap`) so the file's key
    /// order is deterministic and stable across re-recording -- a real
    /// content change shows up as a small diff, not a reshuffle.
    pub entries: BTreeMap<String, Entry>,
    #[serde(default)]
    pub skipped: Vec<SkipRecord>,
    #[serde(default)]
    pub failed: Vec<ConversionFailure>,
}

impl Baseline {
    pub fn new() -> Self {
        Baseline {
            schema_version: SCHEMA_VERSION,
            context: Context::default(),
            entries: BTreeMap::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Baseline, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read baseline {}: {e}", path.display()))?;
        let baseline: Baseline = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse baseline {}: {e}", path.display()))?;
        if baseline.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "baseline {} has schema_version {}, this build of convkit-diff \
                 writes/reads schema_version {SCHEMA_VERSION}",
                path.display(),
                baseline.schema_version
            ));
        }
        Ok(baseline)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("cannot serialise baseline: {e}"))?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, text + "\n")
            .map_err(|e| format!("cannot write baseline {}: {e}", path.display()))
    }
}

/// The `<baseline>.refs/` directory that holds reference output files for
/// the pixel axis. Sits next to the baseline JSON, named after its stem so
/// `baseline.json` gets `baseline.refs/` and `foo/baseline.json` gets
/// `foo/baseline.refs/`.
pub fn refs_dir_for(baseline_path: &Path) -> PathBuf {
    let dir = baseline_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let stem = baseline_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "baseline".to_string());
    dir.join(format!("{stem}.refs"))
}

/// The key an entry is stored under: `"<corpus-relative input> -> <to>"`.
/// A plain string (not a struct) so it doubles as the sort key that keeps
/// the JSON file's entries in a stable, reviewable order.
pub fn entry_key(input_rel: &str, to: &str) -> String {
    format!("{input_rel} -> {to}")
}

/// A filesystem-safe name for an entry's reference file, derived from its
/// key. Not required to be reversible -- `Entry::pixel_ref` is what
/// `compare` actually reads back -- only stable and collision-free for the
/// corpus this ran over.
pub fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
