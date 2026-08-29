pub mod error;
pub mod format;

pub use error::{ConvError, ErrorCode, Remediation, Result};
pub use format::{Format, Kind};

// TODO(task-3): replaced by the real `Backend` enum in `backend.rs`.
pub type Backend = String;
