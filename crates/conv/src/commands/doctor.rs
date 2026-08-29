use convkit_core::{Backend, Source};
use serde_json::json;

use crate::cli::Cli;

const BACKENDS: [Backend; 5] = [
    Backend::Ffmpeg,
    Backend::Ffprobe,
    Backend::Magick,
    Backend::Pandoc,
    Backend::Soffice,
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
                    "managed_install": b.is_managed(),
                }),
                Err(e) => json!({
                    "backend": b,
                    "found": false,
                    "managed_install": b.is_managed(),
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
                    r.version,
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
