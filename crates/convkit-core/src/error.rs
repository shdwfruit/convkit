use serde::Serialize;

use crate::{manifest, Backend, Format, PackageManager};

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
    /// The command line itself is malformed — wrong shape, colliding
    /// outputs, and the like — as opposed to `UnsupportedPair`, which means
    /// the invocation was well-formed but no recipe exists for that pair.
    /// Kept distinct so a `--json` consumer can tell "no backend supports
    /// this conversion" apart from "your invocation doesn't parse"; the
    /// spec fixes exit codes, not error codes, so both still exit 2.
    InvalidInvocation,
}

impl ErrorCode {
    /// Process exit code. Values are fixed by the spec; do not renumber.
    pub fn exit_code(&self) -> i32 {
        match self {
            ErrorCode::ConversionFailed => 1,
            ErrorCode::UnsupportedPair
            | ErrorCode::UnknownFormat
            | ErrorCode::InputNotFound
            | ErrorCode::OutputExists
            | ErrorCode::InvalidInvocation => 2,
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
        ConvError {
            code,
            message: message.into(),
            backend: None,
            remediation: None,
        }
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
            None => {
                format!("unknown format {ext:?}; run `conv capabilities` to list known formats")
            }
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

impl ConvError {
    /// The manual-install command for `backend`: the right command for a
    /// detected package manager, or the official download page when none is
    /// detected — so this is never empty, regardless of what's on PATH.
    fn manual_hint_always_some(backend: Backend) -> String {
        PackageManager::detect()
            .map(|pm| backend.manual_hint(pm).to_string())
            .unwrap_or_else(|| backend.download_hint().to_string())
    }

    /// A required backend could not be resolved anywhere. `remediation.managed`
    /// is only offered when `manifest::has_managed_build(backend)` is true —
    /// that is, when a `conv install <backend>` would actually succeed on
    /// this platform, not merely when `Backend::is_managed()` says a managed
    /// install is architecturally possible for the backend in general.
    ///
    /// This distinction is load-bearing: `Backend::Magick` is
    /// `is_managed() == true`, but the manifest verifies no ImageMagick
    /// asset on any platform, so gating on `is_managed()` alone used to make
    /// this promise `conv install magick`, a command that itself refuses —
    /// the exact primary error path for every image conversion on a machine
    /// without ImageMagick, and a loop for any agent following spec §9's
    /// self-heal contract. See `manifest::has_managed_build`'s docs for the
    /// full reasoning.
    pub fn backend_missing(backend: Backend) -> ConvError {
        let managed = manifest::has_managed_build(backend)
            .then(|| format!("conv install {}", backend.exe_name()));
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!("{} not found", backend.exe_name()),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }

    /// `conv install <backend>` refuses outright, for a backend where
    /// `is_managed()` is false. LibreOffice — today the only case — has no
    /// relocatable binary, so this is a permanent policy refusal, not a
    /// temporary gap: `remediation.managed` is left `None` on purpose,
    /// since offering `conv install <x>` as the fix for `conv install <x>`
    /// refusing would be circular.
    pub fn not_installable(backend: Backend) -> ConvError {
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!(
                "{} has no relocatable build; it can't be installed by conv",
                backend.exe_name()
            ),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed: None,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }

    /// `backend.is_managed()` is true, but `manifest::lookup` has no
    /// verified asset for the platform this process is running on.
    /// Deliberately distinct from an unverified manifest entry: per Task
    /// 14's controller ruling, a missing entry must fail immediately with
    /// this remediation, not after a download, with a checksum error the
    /// user cannot act on.
    pub fn no_managed_build(backend: Backend) -> ConvError {
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!(
                "no managed build of {} is available for this platform",
                backend.exe_name()
            ),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed: None,
                manual: Some(Self::manual_hint_always_some(backend)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_missing_never_leaves_remediation_empty() {
        // Soffice is never managed, and this must hold regardless of
        // whether a package manager happens to be installed on the machine
        // running this test.
        let e = ConvError::backend_missing(Backend::Soffice);
        let remediation = e
            .remediation
            .expect("backend_missing always sets remediation");
        assert_eq!(remediation.managed, None, "soffice is never managed");
        assert!(
            remediation.manual.is_some(),
            "manual must always be Some, even with no package manager detected"
        );
    }

    #[test]
    fn backend_missing_offers_managed_install_for_managed_backends() {
        let e = ConvError::backend_missing(Backend::Pandoc);
        let remediation = e
            .remediation
            .expect("backend_missing always sets remediation");
        if crate::manifest::has_managed_build(Backend::Pandoc) {
            assert_eq!(remediation.managed, Some("conv install pandoc".to_string()));
        } else {
            // No manifest row for this platform (e.g. linux/arm64) -- a
            // managed install genuinely isn't offered here.
            assert_eq!(remediation.managed, None);
        }
        assert!(remediation.manual.is_some());
    }

    /// C1: `Backend::Magick` is `is_managed() == true` (a managed install is
    /// architecturally possible in principle), but the manifest verifies no
    /// ImageMagick asset on any platform — every official release is a
    /// `.7z`, an AppImage, or has no standalone build at all. Before this
    /// fix, `backend_missing` gated `remediation.managed` on `is_managed()`
    /// alone, so it promised `conv install magick`, a command that itself
    /// refuses with "no managed build of magick is available for this
    /// platform" — verified live, same binary: this is deterministic on
    /// every platform this test runs on, unlike a similar assertion against
    /// ffmpeg or pandoc would be (those have real manifest rows on some
    /// platforms).
    #[test]
    fn backend_missing_never_offers_managed_install_when_no_manifest_entry_exists() {
        let e = ConvError::backend_missing(Backend::Magick);
        let remediation = e.remediation.expect("must carry remediation");
        assert_eq!(
            remediation.managed, None,
            "magick has no manifest entry on any platform; `conv install magick` \
             would itself refuse, so promising it here would loop an agent \
             following it"
        );
        assert!(remediation.manual.is_some());
    }

    #[test]
    fn not_installable_never_offers_a_managed_install() {
        let e = ConvError::not_installable(Backend::Soffice);
        assert_eq!(e.code, ErrorCode::BackendMissing);
        let remediation = e.remediation.expect("must carry remediation");
        assert_eq!(remediation.managed, None);
        assert!(remediation.manual.is_some());
    }

    #[test]
    fn no_managed_build_never_offers_a_managed_install_either() {
        let e = ConvError::no_managed_build(Backend::Pandoc);
        assert_eq!(e.code, ErrorCode::BackendMissing);
        let remediation = e.remediation.expect("must carry remediation");
        assert_eq!(
            remediation.managed, None,
            "offering `conv install pandoc` as the fix for `conv install pandoc` \
             finding no manifest entry would be circular"
        );
        assert!(remediation.manual.is_some());
    }

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(ErrorCode::ConversionFailed.exit_code(), 1);
        assert_eq!(ErrorCode::UnsupportedPair.exit_code(), 2);
        assert_eq!(ErrorCode::UnknownFormat.exit_code(), 2);
        assert_eq!(ErrorCode::InputNotFound.exit_code(), 2);
        assert_eq!(ErrorCode::OutputExists.exit_code(), 2);
        assert_eq!(ErrorCode::BackendMissing.exit_code(), 3);
        assert_eq!(ErrorCode::BatchPartialFailure.exit_code(), 4);
        assert_eq!(ErrorCode::InvalidInvocation.exit_code(), 2);
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
