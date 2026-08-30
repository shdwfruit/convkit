use std::path::PathBuf;

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

/// Downloads and verifies a managed backend's asset, placing every binary it
/// contains at its own `Resolver::managed_path` so each is found ahead of
/// `PATH` on the next run — not just the one named by `backend`. On a
/// platform where `backend`'s asset bundles other backends too (today: the
/// Windows ffmpeg/ffprobe zip), those are installed from the exact same
/// download, never fetched a second time; see
/// `convkit_core::install::fetch_and_install`'s own docs for how that's
/// guaranteed. Shared by the `install` subcommand's own `run` below and by
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
///
/// Returns every `(backend, path)` actually installed — always includes
/// `backend` itself, plus any bundled sibling.
pub(crate) fn install_backend(
    cli: &Cli,
    backend: Backend,
) -> Result<Vec<(Backend, std::path::PathBuf)>, ConvError> {
    let asset = manifest::lookup(backend).ok_or_else(|| ConvError::no_managed_build(backend))?;

    if !cli.quiet && !cli.json {
        // Progress goes to stderr, matching the batch progress bar
        // (`indicatif::ProgressBar` also draws there) — stdout is reserved
        // for the final machine-parseable result (or, in `--json` mode, the
        // whole envelope), the same split `render`'s other commands keep.
        eprintln!("downloading {} ...", asset.url);
    }

    install::fetch_and_install(asset, Resolver::managed_path)
}

/// Where `installed` says `backend` itself landed. Falls back to the first
/// entry only if `backend` is somehow absent from `installed` — an
/// invariant `install_backend` always upholds in practice, so this is
/// belt-and-braces against a panic, not a case expected to trigger.
fn primary_path(installed: &[(Backend, PathBuf)], backend: Backend) -> PathBuf {
    installed
        .iter()
        .find(|(b, _)| *b == backend)
        .or_else(|| installed.first())
        .map(|(_, p)| p.clone())
        .unwrap_or_default()
}

/// The lines `run` prints on a successful install, human mode: the
/// requested backend's own result first, then one "also installed ..."
/// line per bundled sibling `install_backend` placed alongside it (empty
/// when nothing else was bundled in). A pure function of `installed` so the
/// exact wording — including the "also installed" report this task exists
/// to add — is unit-testable without a real download.
fn success_lines_human(backend: Backend, installed: &[(Backend, PathBuf)]) -> Vec<String> {
    let mut lines = vec![format!(
        "installed {} -> {}",
        backend.exe_name(),
        primary_path(installed, backend).display()
    )];
    lines.extend(
        installed
            .iter()
            .filter(|(b, _)| *b != backend)
            .map(|(b, p)| format!("also installed {} -> {}", b.exe_name(), p.display())),
    );
    lines
}

/// The `--json` success envelope. `backend`/`path` keep their original
/// shape and meaning — the backend asked for, and where it landed — so an
/// existing consumer reading just those two fields sees no change.
/// `installed` is additive: every binary this download actually placed,
/// `backend`'s bundled siblings included.
fn success_envelope(backend: Backend, installed: &[(Backend, PathBuf)]) -> serde_json::Value {
    let installed_json: Vec<_> = installed
        .iter()
        .map(|(b, p)| json!({ "backend": b, "path": p }))
        .collect();
    json!({
        "ok": true,
        "backend": backend,
        "path": primary_path(installed, backend),
        "installed": installed_json,
    })
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
        Ok(installed) => {
            if cli.json {
                let envelope = success_envelope(backend, &installed);
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            } else {
                for line in success_lines_human(backend, &installed) {
                    println!("{line}");
                }
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

    /// The ordinary case: one member installed, nothing bundled — a single
    /// "installed ..." line and no "also installed" noise.
    #[test]
    fn success_lines_human_reports_just_the_one_binary_when_nothing_is_bundled() {
        let installed = vec![(Backend::Typst, PathBuf::from("/managed/typst"))];
        let lines = success_lines_human(Backend::Typst, &installed);
        assert_eq!(lines, vec!["installed typst -> /managed/typst"]);
    }

    /// The mechanism this task adds: installing ffmpeg must also report
    /// ffprobe landing, not silently place a second file.
    #[test]
    fn success_lines_human_reports_a_bundled_sibling() {
        let installed = vec![
            (Backend::Ffmpeg, PathBuf::from("/managed/ffmpeg")),
            (Backend::Ffprobe, PathBuf::from("/managed/ffprobe")),
        ];
        let lines = success_lines_human(Backend::Ffmpeg, &installed);
        assert_eq!(
            lines,
            vec![
                "installed ffmpeg -> /managed/ffmpeg",
                "also installed ffprobe -> /managed/ffprobe",
            ]
        );
    }

    /// Symmetric: `conv install ffprobe` on the same bundle must report
    /// ffprobe as the primary result and ffmpeg as the bonus, not the other
    /// way around — the user asked for ffprobe, so that's what "installed"
    /// names.
    #[test]
    fn success_lines_human_names_the_requested_backend_first_regardless_of_vec_order() {
        let installed = vec![
            (Backend::Ffmpeg, PathBuf::from("/managed/ffmpeg")),
            (Backend::Ffprobe, PathBuf::from("/managed/ffprobe")),
        ];
        let lines = success_lines_human(Backend::Ffprobe, &installed);
        assert_eq!(
            lines,
            vec![
                "installed ffprobe -> /managed/ffprobe",
                "also installed ffmpeg -> /managed/ffmpeg",
            ]
        );
    }

    /// `--json`'s `backend`/`path` keep their pre-existing shape (I3: no
    /// contract break for a consumer that only reads those two fields), and
    /// `installed` is the additive array carrying the rest.
    #[test]
    fn success_envelope_keeps_backend_and_path_and_adds_installed() {
        let installed = vec![
            (Backend::Ffmpeg, PathBuf::from("/managed/ffmpeg")),
            (Backend::Ffprobe, PathBuf::from("/managed/ffprobe")),
        ];
        let v = success_envelope(Backend::Ffmpeg, &installed);
        assert_eq!(v["ok"], true);
        assert_eq!(v["backend"], "ffmpeg");
        assert_eq!(v["path"], "/managed/ffmpeg");
        assert_eq!(v["installed"].as_array().unwrap().len(), 2);
        assert_eq!(v["installed"][0]["backend"], "ffmpeg");
        assert_eq!(v["installed"][0]["path"], "/managed/ffmpeg");
        assert_eq!(v["installed"][1]["backend"], "ffprobe");
        assert_eq!(v["installed"][1]["path"], "/managed/ffprobe");
    }

    #[test]
    fn success_envelope_installed_array_has_one_entry_when_nothing_is_bundled() {
        let installed = vec![(Backend::Typst, PathBuf::from("/managed/typst"))];
        let v = success_envelope(Backend::Typst, &installed);
        assert_eq!(v["installed"].as_array().unwrap().len(), 1);
    }
}
