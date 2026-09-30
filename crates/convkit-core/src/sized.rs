//! The `--max-size` layer: turns a probed source and a size target into a
//! plan, and holds the arithmetic and the wording the executor and the CLI
//! share. The choosing itself is `budget.rs`; this module is everything
//! around it.

use serde::{Serialize, Serializer};

use crate::budget::SizedChoice;
use crate::size::UnitFamily;
use crate::Format;

/// How a sized conversion gets under its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// The source already fits and is the target's own container: copied
    /// byte for byte.
    Copy,
    /// The source already fits and its streams suit the target container:
    /// stream-copied, then re-encoded only if the copy comes out over.
    Remux,
    /// Two-pass encode at the chosen settings, retried while over.
    Encode,
}

/// What a sized plan decided, carried on `ConversionPlan` so `--dry-run`,
/// the executor and the CLI's confirmation prompt all see the same answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizingPlan {
    pub target_bytes: u64,
    /// The target as the user wrote it, for sentences: `10 MB`.
    pub target_label: String,
    pub family: UnitFamily,
    pub strategy: Strategy,
    /// The settings chosen, for `Strategy::Encode`.
    pub choice: Option<SizedChoice>,
    /// The extreme or predicted-over sentence, when the choice is extreme.
    pub warning: Option<String>,
    /// When extreme: the suggested target's spelling (`40mb`).
    pub suggested: Option<String>,
    /// The source file's size, as ffprobe reported it.
    pub source_bytes: Option<u64>,
}

/// What a sized conversion actually did, on `Outcome` and in `--json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SizingReport {
    pub target_bytes: u64,
    pub family: UnitFamily,
    pub strategy: Strategy,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(serialize_with = "optional_rational")]
    pub fps: Option<(u32, u32)>,
    pub video_bps: Option<u64>,
    /// One entry per audio track, bits per second.
    pub audio_bps: Vec<u64>,
    /// Pass-2 runs for an encode (1 = fitted first time); 1 for a remux;
    /// 0 for a copy.
    pub attempts: u32,
    pub cost: Option<f64>,
    pub over_target: bool,
    pub suggested: Option<String>,
}

// Spelled `std::result::Result` so a later import of the crate's
// one-parameter `Result` alias into this module cannot shadow it.
fn optional_rational<S: Serializer>(
    r: &Option<(u32, u32)>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match r {
        Some((n, d)) => s.serialize_str(&format!("{n}/{d}")),
        None => s.serialize_none(),
    }
}

/// The containers `--max-size` can size: the four video targets.
pub fn is_video_target(to: Format) -> bool {
    matches!(to, Format::Mp4 | Format::Mov | Format::Mkv | Format::Webm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_video_containers_are_sized() {
        for f in [Format::Mp4, Format::Mov, Format::Mkv, Format::Webm] {
            assert!(is_video_target(f), "{f:?}");
        }
        for f in [Format::Gif, Format::Avi, Format::Mp3, Format::Jpg] {
            assert!(!is_video_target(f), "{f:?}");
        }
    }

    fn report(fps: Option<(u32, u32)>) -> SizingReport {
        SizingReport {
            target_bytes: 10_000_000,
            family: UnitFamily::Decimal,
            strategy: Strategy::Encode,
            width: Some(1280),
            height: Some(720),
            fps,
            video_bps: Some(900_000),
            audio_bps: vec![96_000],
            attempts: 1,
            cost: Some(1.5),
            over_target: false,
            suggested: None,
        }
    }

    /// `--json` output is a published contract: the rate is an `N/D` string,
    /// the enums are snake_case, and an unknown rate is `null`, not absent.
    #[test]
    fn a_sizing_report_serialises_in_its_json_shape() {
        let v = serde_json::to_value(report(Some((30_000, 1_001)))).unwrap();
        assert_eq!(v["fps"], "30000/1001", "{v}");
        assert_eq!(v["strategy"], "encode", "{v}");
        assert_eq!(v["family"], "decimal", "{v}");

        let v = serde_json::to_value(report(None)).unwrap();
        assert!(v.get("fps").is_some_and(|f| f.is_null()), "{v}");
    }
}
