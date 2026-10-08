pub mod backend;
pub mod backend_overrides;
pub mod budget;
pub mod error;
pub mod exec;
pub mod format;
pub mod frames;
pub mod install;
pub mod manifest;
mod media;
pub mod metadata;
pub mod plan;
pub mod probe;
mod procutil;
pub mod recipe;
pub mod registry;
pub mod resolve;
pub mod size;
pub mod sized;
pub mod trim;
mod video;
pub mod winpath;

pub use backend::{Backend, PackageManager};
pub use backend_overrides::BackendOverrides;
pub use error::{manual_hint_for, ConvError, ErrorCode, Remediation, Result};
pub use exec::{BackendOutput, Event, Outcome, Request};
pub use format::{Format, Kind};
pub use plan::build as build_plan;
pub use plan::{ConversionPlan, PlannedStep};
pub use probe::MediaProbe;
pub use recipe::{Arg, OutputMode, Recipe, Step, Tuning};
pub use resolve::{AvailableBackends, ResolvedBackend, Resolver, Source};
pub use video::{
    confirmation_error as upscale_confirmation_error, resolve as resolve_video, Enlargement,
    ResolvedVideo,
};
