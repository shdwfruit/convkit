pub mod archive;
pub mod ascii;
pub mod backend;
pub mod backend_overrides;
pub mod beautify;
pub mod budget;
pub mod crop;
pub mod data;
pub mod denoise;
pub mod error;
pub mod exec;
mod exif;
pub mod format;
pub mod frames;
mod icon;
pub mod info;
pub mod install;
mod jpeg;
mod layout;
pub mod manifest;
mod media;
pub mod merge;
pub mod metadata;
mod mov;
pub mod office;
pub mod offline;
pub mod palette;
pub mod pdf;
pub mod picture;
pub mod plan;
pub mod probe;
mod procutil;
pub mod qr;
pub mod recipe;
pub mod registry;
pub mod resolve;
pub mod rotate;
mod sheet_pdf;
pub mod size;
pub mod sized;
pub mod soundtrack;
pub mod speech;
pub mod speed;
pub mod stills;
pub mod subtitles;
pub mod table;
pub mod text;
pub mod tools;
pub mod trim;
mod video;
pub mod wifi;
pub mod winpath;
mod xmp;

pub use backend::{Backend, ManagedLayout, PackageManager};
pub use backend_overrides::BackendOverrides;
pub use error::{manual_hint_for, ConvError, ErrorCode, Remediation, Result};
pub use exec::{BackendOutput, Event, Outcome, Request};
pub use format::{Format, Kind};
pub use plan::build as build_plan;
pub use plan::{ConversionPlan, PlannedStep};
pub use probe::MediaProbe;
pub use recipe::{Arg, OutputMode, RateStyle, Recipe, Staged, Step, Tuning};
pub use resolve::{AvailableBackends, ResolvedBackend, Resolver, Source};
pub use video::{
    confirmation_error as upscale_confirmation_error, resolve as resolve_video, Enlargement,
    ResolvedVideo,
};

/// `#[serde(serialize_with)]` for a path field: its text, with any byte that
/// is not UTF-8 replaced, where serde's own `Path` impl fails and `json!`
/// turns the failure into a panic. `lossy_paths` and `lossy_opt_path` are
/// the same for a list and an option.
pub fn lossy_path<S: serde::Serializer>(
    p: &std::path::Path,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&p.to_string_lossy())
}

pub fn lossy_paths<S: serde::Serializer>(
    paths: &[std::path::PathBuf],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(paths.iter().map(|p| p.to_string_lossy()))
}

pub fn lossy_opt_path<S: serde::Serializer>(
    p: &Option<std::path::PathBuf>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match p {
        Some(p) => s.serialize_some(&p.to_string_lossy()),
        None => s.serialize_none(),
    }
}
