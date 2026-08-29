use convkit_core::{install, manifest, Backend, ConvError, ErrorCode, Resolver};
use serde_json::json;

use crate::cli::Cli;
use crate::render;

/// Maps a CLI argument to a `Backend`. Accepts each backend's canonical
/// `exe_name()` plus a couple of names people are more likely to type.
fn parse_backend(name: &str) -> Option<Backend> {
    match name.to_ascii_lowercase().as_str() {
        "ffmpeg" => Some(Backend::Ffmpeg),
        "ffprobe" => Some(Backend::Ffprobe),
        "magick" | "imagemagick" => Some(Backend::Magick),
        "soffice" | "libreoffice" => Some(Backend::Soffice),
        "pandoc" => Some(Backend::Pandoc),
        "typst" => Some(Backend::Typst),
        _ => None,
    }
}

/// Downloads and verifies a managed backend, placing it at
/// `Resolver::managed_path(backend)` so it's found ahead of `PATH` on the
/// next run. Shared by the `install` subcommand's own `run` below and by
/// `commands/convert.rs`'s install-and-retry prompt (Part 1) — both must go
/// through this exact function, never a re-implementation, so both callers
/// get identical behaviour (the same progress line, checksum verification,
/// and atomic install) and `convkit-core` stays the only thing that ever
/// touches the network.
///
/// Callers are expected to have already checked `manifest::has_managed_build`
/// (`run` does, via `backend.is_managed()` plus the lookup below; the Part 1
/// prompt gates on `manifest::has_managed_build` directly before ever
/// offering to call this), but this re-derives the asset itself and fails
/// the same way `run` always did if that invariant is somehow violated,
/// rather than assuming it holds.
pub(crate) fn install_backend(
    cli: &Cli,
    backend: Backend,
) -> Result<std::path::PathBuf, ConvError> {
    let asset = manifest::lookup(backend).ok_or_else(|| ConvError::no_managed_build(backend))?;

    if !cli.quiet && !cli.json {
        // Progress goes to stderr, matching the batch progress bar
        // (`indicatif::ProgressBar` also draws there) — stdout is reserved
        // for the final machine-parseable result (or, in `--json` mode, the
        // whole envelope), the same split `render`'s other commands keep.
        eprintln!("downloading {} ...", asset.url);
    }

    let dest = Resolver::managed_path(backend);
    install::fetch_and_install(asset, &dest)
}

/// Refuses two kinds of request before touching the network: a backend name
/// this CLI doesn't recognise at all, and a backend where
/// `Backend::is_managed()` is false (today, only `soffice` — LibreOffice has
/// no relocatable binary). `install_backend` itself refuses a third kind —
/// a recognised, managed backend with no verified manifest entry for the
/// running platform — for the same reason: `manifest::lookup` returning
/// `None` means this hasn't been verified to work, and a download that
/// later fails its checksum is a worse failure mode than refusing up front.
pub fn run(cli: &Cli, backend_name: &str) -> i32 {
    let backend = match parse_backend(backend_name) {
        Some(b) => b,
        None => {
            let e = ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "unknown backend {backend_name:?}; expected one of: \
                     ffmpeg, ffprobe, magick, pandoc, soffice, typst"
                ),
            );
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };

    if !backend.is_managed() {
        let e = ConvError::not_installable(backend);
        render::print_error(cli.json, &e);
        return e.code.exit_code();
    }

    match install_backend(cli, backend) {
        Ok(path) => {
            if cli.json {
                let envelope = json!({ "ok": true, "backend": backend, "path": path });
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                println!("installed {} -> {}", backend.exe_name(), path.display());
            }
            0
        }
        Err(e) => {
            render::print_error(cli.json, &e);
            e.code.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_recognises_every_canonical_exe_name() {
        for (name, backend) in [
            ("ffmpeg", Backend::Ffmpeg),
            ("ffprobe", Backend::Ffprobe),
            ("magick", Backend::Magick),
            ("soffice", Backend::Soffice),
            ("pandoc", Backend::Pandoc),
            ("typst", Backend::Typst),
        ] {
            assert_eq!(parse_backend(name), Some(backend));
        }
    }

    #[test]
    fn parse_backend_is_case_insensitive() {
        assert_eq!(parse_backend("FFmpeg"), Some(Backend::Ffmpeg));
    }

    #[test]
    fn parse_backend_rejects_nonsense() {
        assert_eq!(parse_backend("not-a-real-backend"), None);
    }
}
