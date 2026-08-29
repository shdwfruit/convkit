use convkit_core::{manifest, Backend, Source};
use serde_json::json;

use crate::cli::Cli;

const BACKENDS: [Backend; 6] = [
    Backend::Ffmpeg,
    Backend::Ffprobe,
    Backend::Magick,
    Backend::Pandoc,
    Backend::Soffice,
    Backend::Typst,
];

/// `ResolvedBackend`/`Source` don't derive `Serialize` (the core crate never
/// prints, so it has no reason to), so this maps `Source` to the string
/// doctor's own output uses.
fn source_str(s: Source) -> &'static str {
    match s {
        Source::Override => "override",
        Source::Env => "env",
        Source::Managed => "managed",
        Source::Path => "path",
        Source::WellKnown => "well_known",
    }
}

/// Width of the human-mode version column, in characters.
const VERSION_COLUMN_WIDTH: usize = 10;

/// Caps `s` at `width` displayed characters, appending an ellipsis when it
/// had to cut something off. `{:<N}` pads a short string but never caps a
/// long one, and `convkit_core::Resolver::resolve`'s version string is
/// backend-controlled text with no length guarantee — `extract_version_token`
/// keeps it short in practice, but this is the layer that makes the table
/// *structurally* immune to whatever a backend's banner looks like, rather
/// than merely usually short.
fn truncate_column(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut truncated: String = s.chars().take(keep).collect();
    truncated.push('…');
    truncated
}

/// Reports every backend's install state. Always exits 0 — a missing tool is
/// this command's whole reason to exist, not a failure of the command
/// itself — and never runs an install command: it only ever prints one, via
/// the same remediation `Resolver::resolve` already computes for a missing
/// backend (which itself calls `PackageManager::detect()` to pick the
/// manual hint).
pub fn run(cli: &Cli) -> i32 {
    let resolver = cli.resolver();

    if cli.json {
        let arr: Vec<serde_json::Value> = BACKENDS
            .iter()
            .map(|&b| match resolver.resolve(b) {
                Ok(r) => json!({
                    "backend": b,
                    "found": true,
                    "path": r.path,
                    "version": r.version,
                    "source": source_str(r.source),
                    // `has_managed_build`, not the raw `is_managed()` policy
                    // bit: whether `conv install <b>` would actually
                    // succeed on this platform (C1) — `magick` is
                    // `is_managed() == true` but has no verified manifest
                    // entry anywhere, so it must report `false` here too.
                    "managed_install": manifest::has_managed_build(b),
                }),
                Err(e) => json!({
                    "backend": b,
                    "found": false,
                    "managed_install": manifest::has_managed_build(b),
                    "remediation": e.remediation,
                }),
            })
            .collect();
        let envelope = json!({ "ok": true, "backends": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        for &b in &BACKENDS {
            match resolver.resolve(b) {
                Ok(r) => println!(
                    "{:<9} {:<10} {:<28} ({})",
                    b.exe_name(),
                    truncate_column(&r.version, VERSION_COLUMN_WIDTH),
                    r.path.display(),
                    source_str(r.source).to_uppercase(),
                ),
                Err(e) => {
                    let rem = e.remediation.unwrap_or(convkit_core::Remediation {
                        managed: None,
                        manual: None,
                    });
                    let managed = rem.managed.unwrap_or_else(|| "manual install only".into());
                    let manual = rem.manual.unwrap_or_default();
                    println!("{:<9} missing    {managed}  |  {manual}", b.exe_name());
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_pass_through_unchanged() {
        assert_eq!(truncate_column("9.0", 10), "9.0");
        assert_eq!(truncate_column("", 10), "");
    }

    #[test]
    fn a_string_exactly_at_width_is_not_truncated() {
        assert_eq!(truncate_column("1234567890", 10), "1234567890");
    }

    /// This is the exact defect the coordinator reported: `version_of` (once
    /// fixed to use the right flag) returned ffmpeg's whole ~90-character
    /// banner line, which `{:<10}` pads but never caps, blowing out every
    /// column after it.
    #[test]
    fn a_long_real_ffmpeg_version_token_is_capped_at_the_column_width() {
        let long = "9.0-full_build-www.gyan.dev";
        let out = truncate_column(long, 10);
        assert_eq!(out.chars().count(), 10, "{out:?}");
        assert!(out.ends_with('…'), "{out:?}");
        assert!(
            long.starts_with(&out[..out.len() - '…'.len_utf8()]),
            "{out:?}"
        );
    }
}
