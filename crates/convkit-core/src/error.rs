use serde::Serialize;

use crate::Format;

pub type Result<T> = std::result::Result<T, ConvError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The pair is well-formed but no recipe exists.
    UnsupportedPair,
    /// An extension we do not recognise at all.
    UnknownFormat,
    /// A required backend executable could not be found.
    BackendMissing,
    /// The backend ran and failed, or produced no usable output.
    ConversionFailed,
    InputNotFound,
    OutputExists,
    /// Batch finished, but at least one item failed.
    BatchPartialFailure,
}

impl ErrorCode {
    /// Process exit code. Values are fixed by the spec; do not renumber.
    pub fn exit_code(&self) -> i32 {
        match self {
            ErrorCode::ConversionFailed => 1,
            ErrorCode::UnsupportedPair
            | ErrorCode::UnknownFormat
            | ErrorCode::InputNotFound
            | ErrorCode::OutputExists => 2,
            ErrorCode::BackendMissing => 3,
            ErrorCode::BatchPartialFailure => 4,
        }
    }
}

/// How the user can fix this failure themselves. `managed` is a convkit
/// subcommand; `manual` is a package-manager command we print but never run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Remediation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConvError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<crate::Backend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<Remediation>,
}

impl ConvError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ConvError { code, message: message.into(), backend: None, remediation: None }
    }

    pub fn unsupported_pair(from: Format, to: Format) -> Self {
        ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "converting {} to {} is not supported; run `conv capabilities` to list supported pairs",
                from.ext(),
                to.ext()
            ),
        )
    }

    pub fn unknown_format(ext: &str) -> Self {
        let msg = match Format::suggest(ext) {
            Some(f) => format!("unknown format {ext:?} — did you mean {:?}?", f.ext()),
            None => format!("unknown format {ext:?}; run `conv capabilities` to list known formats"),
        };
        ConvError::new(ErrorCode::UnknownFormat, msg)
    }
}

impl std::fmt::Display for ConvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConvError {}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Format;

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(ErrorCode::ConversionFailed.exit_code(), 1);
        assert_eq!(ErrorCode::UnsupportedPair.exit_code(), 2);
        assert_eq!(ErrorCode::UnknownFormat.exit_code(), 2);
        assert_eq!(ErrorCode::InputNotFound.exit_code(), 2);
        assert_eq!(ErrorCode::OutputExists.exit_code(), 2);
        assert_eq!(ErrorCode::BackendMissing.exit_code(), 3);
        assert_eq!(ErrorCode::BatchPartialFailure.exit_code(), 4);
    }

    #[test]
    fn unknown_format_carries_a_suggestion_in_its_message() {
        let e = ConvError::unknown_format("mp3v");
        assert_eq!(e.code, ErrorCode::UnknownFormat);
        assert!(e.message.contains("did you mean"), "{}", e.message);
        assert!(e.message.contains("mp3"), "{}", e.message);
    }

    #[test]
    fn unsupported_pair_names_both_formats() {
        let e = ConvError::unsupported_pair(Format::Pdf, Format::Mp4);
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert!(e.message.contains("pdf"));
        assert!(e.message.contains("mp4"));
    }

    #[test]
    fn serialises_to_the_documented_json_envelope() {
        let e = ConvError {
            code: ErrorCode::BackendMissing,
            message: "ffmpeg not found".into(),
            backend: None,
            remediation: Some(Remediation {
                managed: Some("conv install ffmpeg".into()),
                manual: Some("winget install Gyan.FFmpeg".into()),
            }),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "backend_missing");
        assert_eq!(v["remediation"]["managed"], "conv install ffmpeg");
    }
}
