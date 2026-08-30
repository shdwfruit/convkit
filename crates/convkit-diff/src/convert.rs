//! Runs one conversion through convkit-core's real `exec::run` -- the exact
//! engine `conv` itself uses -- and inspects the result. This is the one
//! place record and compare share: both need "convert this input to this
//! format, then describe what came out" and must describe it identically,
//! or a diff between them would be comparing apples to oranges.

use std::path::{Path, PathBuf};

use convkit_core::{exec, Backend, Format, Resolver};
use sha2::{Digest, Sha256};

use crate::baseline::{Entry, GifInfo};
use crate::{gif_colors, inspect, scope};

pub struct FreshResult {
    pub entry: Entry,
    /// Where the fresh output currently lives. Still on disk when this is
    /// returned; the caller (record copies it into `.refs/`, compare diffs
    /// it against the stored ref) is responsible for it from here, and it's
    /// removed along with the whole scratch directory it lives in once the
    /// caller is done.
    pub output_path: PathBuf,
}

/// The resolved backends every conversion in a `record`/`compare` run
/// shares, bundled so `convert_and_inspect` doesn't have to take them as
/// three separate parameters.
pub struct Backends<'a> {
    pub resolver: &'a Resolver,
    pub magick: &'a Path,
    pub ffmpeg: Option<&'a Path>,
}

/// Converts `input` (`from` -> `to`) into `scratch_dir`, then builds the
/// same [`Entry`] shape both `record` and `compare` use. `input_rel` is the
/// corpus-relative path recorded on the entry.
pub fn convert_and_inspect(
    backends: &Backends,
    input: &Path,
    input_rel: &str,
    from: Format,
    to: Format,
    scratch_dir: &Path,
    unique_name: &str,
) -> Result<FreshResult, String> {
    let magick = backends.magick;
    let output_path = scratch_dir.join(format!("{unique_name}.{}", to.ext()));

    let req = exec::Request {
        from,
        to,
        inputs: vec![input.to_path_buf()],
        output: output_path.clone(),
        overwrite: true,
        tuning: Default::default(),
    };
    let outcome = exec::run(&req, backends.resolver, &mut |_| {}).map_err(|e| e.message)?;

    let bytes = std::fs::read(&outcome.output).map_err(|e| {
        format!(
            "cannot read conversion output {}: {e}",
            outcome.output.display()
        )
    })?;
    let size_bytes = bytes.len() as u64;
    let output_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    let backend_strs = outcome
        .backends
        .iter()
        .map(|(b, v)| format!("{} {v}", b.exe_name()))
        .collect();

    let mut dimensions = None;
    let mut colorspace = None;
    let mut orientation = None;
    let mut icc = None;
    let mut gif = None;

    if scope::is_magick_inspectable(to) {
        let snap = inspect::snapshot(magick, &outcome.output);
        dimensions = snap.dimensions;
        colorspace = snap.colorspace;
        orientation = snap.orientation;
        icc = snap.icc;

        if to == Format::Gif {
            let frame_count = inspect::frame_count(magick, &outcome.output);
            let unique_colors = backends
                .ffmpeg
                .and_then(|ffmpeg| gif_colors::unique_colors(ffmpeg, &outcome.output));
            if let (Some(frame_count), Some(unique_colors)) = (frame_count, unique_colors) {
                gif = Some(GifInfo {
                    frame_count,
                    unique_colors,
                });
            }
        }
    }

    let entry = Entry {
        input: input_rel.to_string(),
        from: from.ext().to_string(),
        to: to.ext().to_string(),
        backends: backend_strs,
        size_bytes,
        output_sha256,
        dimensions,
        colorspace,
        orientation,
        icc,
        gif,
        pixel_ref: None,
    };

    Ok(FreshResult {
        entry,
        output_path: outcome.output,
    })
}

/// Backend version strings for the harness's own `Baseline::context`, e.g.
/// `{"magick": "7.1.2-29", "ffmpeg": "9.0-full_build-www.gyan.dev"}`.
/// Best-effort: a backend that fails to resolve here just doesn't appear,
/// since this is informational context, not something a run should fail
/// over.
pub fn backend_versions(resolver: &Resolver) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for backend in [Backend::Magick, Backend::Ffmpeg] {
        if let Ok(resolved) = resolver.resolve(backend) {
            map.insert(backend.exe_name().to_string(), resolved.version);
        }
    }
    map
}
