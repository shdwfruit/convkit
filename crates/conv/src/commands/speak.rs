//! `conv speak [TEXT] [OUT]`: text read aloud into a sound file.
//!
//! The input works as in the other text tools: the words given, `-` or a
//! pipe for what is piped in, `-f FILE` for a file, `--paste` for the
//! clipboard. The words are written to a passing .txt file and read aloud
//! by the ordinary txt -> OUT conversion, so `--dry-run`, `--json`,
//! `--copy` and every note work as they do there.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use convkit_core::speech::{self, Engine, Quality};
use convkit_core::{registry, ConvError, ErrorCode, Format};
use serde_json::json;

use crate::cli::{Cli, SpeakArgs};
use crate::clipboard::Got;
use crate::commands::convert;
use crate::{input, output, paste, render};

/// What is read aloud.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// Words, and how the results name them.
    Text(String, &'static str),
    /// A file to read.
    File(PathBuf),
}

pub fn run(cli: &Cli, args: &SpeakArgs) -> i32 {
    if args.voices {
        return voices(cli, args);
    }
    let planned = source(args).and_then(|(source, out, note)| {
        let stem = match &source {
            Source::File(f) => f.file_stem().map_or_else(
                || "speech".to_string(),
                |s| s.to_string_lossy().into_owned(),
            ),
            Source::Text(..) => "speech".to_string(),
        };
        let path = output_path(out.as_deref(), &stem, args.outdir.as_deref())?;
        Ok((source, path, note))
    });
    let (source, path, note) = match planned {
        Ok(p) => p,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    if let (Some(note), false) = (note, cli.json || cli.quiet) {
        eprintln!("  note  {}", render::safe_for_terminal(&note));
    }
    // The words are a file only for as long as the run.
    let written = match &source {
        Source::Text(text, _) => match write_text(text) {
            Ok(w) => Some(w),
            Err(e) => {
                render::print_error(cli.json, &e);
                return e.code.exit_code();
            }
        },
        Source::File(_) => None,
    };
    let input = match (&source, &written) {
        (Source::File(f), _) => f.clone(),
        (_, Some((_, p))) => p.clone(),
        _ => unreachable!("words are always written"),
    };
    if !args.dry_run {
        if let Err(e) = output::ensure_dir(args.outdir.as_deref()) {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    }
    let mut run = cli.clone();
    run.command = None;
    run.paths = vec![input.clone(), path];
    run.outdir = None;
    run.to = None;
    run.paste = false;
    run.voice = args.voice.clone();
    run.rate = args.rate;
    run.overwrite = args.overwrite;
    run.dry_run = args.dry_run;
    run.copy = args.copy;
    let passing = match &source {
        Source::Text(_, name) => Some((input.as_path(), *name)),
        Source::File(_) => None,
    };
    convert::convert_as(&run, passing)
}

/// What to read, the OUT typed, and a note to print first.
fn source(args: &SpeakArgs) -> Result<(Source, Option<PathBuf>, Option<String>), ConvError> {
    let refused = |m: String| ConvError::new(ErrorCode::InvalidInvocation, m);
    // With -f or --paste, the one path given is OUT.
    let only_out = || -> Result<Option<PathBuf>, ConvError> {
        match (&args.text, &args.output) {
            (Some(_), Some(_)) => Err(refused(
                "the text comes from elsewhere here; give at most the file to write".to_string(),
            )),
            (Some(t), None) => Ok(Some(PathBuf::from(t))),
            (None, out) => Ok(out.clone()),
        }
    };
    if args.paste {
        let out = only_out()?;
        let source = match paste::get()? {
            Got::Text(t) => Source::Text(words(t)?, "the pasted text"),
            Got::Files(files) => match files.as_slice() {
                [one] => Source::File(readable(one)?),
                many => {
                    return Err(refused(format!(
                        "the clipboard holds {} files; conv speak reads one",
                        many.len()
                    )))
                }
            },
            Got::Picture { .. } => {
                return Err(refused(
                    "the clipboard holds a picture, not text to read".to_string(),
                ))
            }
        };
        return Ok((source, out, None));
    }
    if let Some(f) = &args.file {
        return Ok((Source::File(readable(f)?), only_out()?, None));
    }
    let piped = args.text.as_deref() == Some("-")
        || (args.text.is_none() && !std::io::stdin().is_terminal());
    if piped {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| refused(format!("couldn't read what was piped in: {e}")))?;
        return Ok((
            Source::Text(words(text)?, "the piped text"),
            args.output.clone(),
            None,
        ));
    }
    let Some(text) = &args.text else {
        return Err(ConvError {
            code: ErrorCode::InvalidInvocation,
            message: "say what to read".to_string(),
            backend: None,
            remediation: Some(convkit_core::Remediation {
                managed: None,
                manual: Some(
                    "conv speak \"some text\", -f notes.txt, - for a pipe, or --paste".to_string(),
                ),
            }),
        });
    };
    // A file's name is read as the words it is; that is seldom what was
    // meant, so say how to read the file.
    let note = Path::new(text)
        .is_file()
        .then(|| format!("{text} is a file, and this reads its name; -f {text} reads the file."));
    Ok((
        Source::Text(words(text.clone())?, "the text"),
        args.output.clone(),
        note,
    ))
}

/// The words, refused when there are none.
fn words(text: String) -> Result<String, ConvError> {
    if text.trim().is_empty() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "there is no text to read aloud",
        ));
    }
    Ok(text)
}

/// A file conv can read aloud.
fn readable(file: &Path) -> Result<PathBuf, ConvError> {
    match Format::from_path(file) {
        Some(f) if registry::speaks(f, Format::Mp3) => Ok(file.to_path_buf()),
        _ => Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "conv reads txt, md, html, docx, odt and rtf files aloud, and {} is none of them",
                file.display()
            ),
        )),
    }
}

/// The file to write: OUT as typed, `STEM.EXT` for a bare `.ext`, or
/// `STEM.mp3`, inside `-o`'s directory when one is given.
fn output_path(
    out: Option<&Path>,
    stem: &str,
    outdir: Option<&Path>,
) -> Result<PathBuf, ConvError> {
    let name = match out {
        None => PathBuf::from(format!("{stem}.mp3")),
        Some(p) if p.is_dir() => {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{} is a directory; to write {stem}.mp3 into it, pass -o {}",
                    p.display(),
                    p.display()
                ),
            ))
        }
        Some(p) if input::is_extension_shorthand(p) => {
            PathBuf::from(format!("{stem}{}", p.display()))
        }
        Some(p) => p.to_path_buf(),
    };
    let path = input::in_outdir(&name, outdir)?;
    let format = input::output_format_of(&path)?;
    if !registry::speaks(Format::Txt, format) {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!(
                "text is read aloud into m4a, mp3, wav or aiff, not .{}",
                format.ext()
            ),
        ));
    }
    Ok(path)
}

/// Writes the words to `speech.txt` in a folder of its own.
fn write_text(text: &str) -> Result<(tempfile::TempDir, PathBuf), ConvError> {
    let (dir, path) = output::private_temp("speech.txt")?;
    std::fs::write(&path, text).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("cannot write the text to read: {e}"),
        )
    })?;
    Ok((dir, path))
}

/// `conv speak --voices`: the voices `--voice` takes, for the engine it
/// would use.
fn voices(cli: &Cli, args: &SpeakArgs) -> i32 {
    let engine = Engine::for_voice(args.voice.as_deref());
    let listed = speech::voices(&cli.resolver(), engine);
    let list = match listed {
        Ok(list) => list,
        Err(e) => {
            render::print_error(cli.json, &e);
            return e.code.exit_code();
        }
    };
    if cli.json {
        let v = json!({ "ok": true, "engine": engine, "voices": list });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return 0;
    }
    let width = list
        .iter()
        .map(|v| v.name.chars().count())
        .max()
        .unwrap_or(0);
    for v in &list {
        let quality = match v.quality {
            Quality::Premium => "  premium",
            Quality::Enhanced => "  enhanced",
            Quality::Standard => "",
        };
        println!("{:<width$}  {}{quality}", v.name, v.language, width = width);
    }
    if engine == Engine::Say && !cli.quiet && list.iter().all(|v| v.quality == Quality::Standard) {
        eprintln!("  note  {}", speech::NO_BETTER_VOICE_NOTE);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: Option<&str>, output: Option<&str>) -> SpeakArgs {
        SpeakArgs {
            text: text.map(str::to_string),
            output: output.map(PathBuf::from),
            file: None,
            paste: false,
            voice: None,
            rate: None,
            voices: false,
            outdir: None,
            overwrite: false,
            dry_run: false,
            copy: false,
        }
    }

    #[test]
    fn without_out_it_writes_speech_mp3_here() {
        assert_eq!(
            output_path(None, "speech", None).unwrap(),
            PathBuf::from("speech.mp3")
        );
        assert_eq!(
            output_path(Some(Path::new(".m4a")), "notes", None).unwrap(),
            PathBuf::from("notes.m4a")
        );
        assert_eq!(
            output_path(Some(Path::new("hi.aiff")), "speech", Some(Path::new("out"))).unwrap(),
            Path::new("out").join("hi.aiff")
        );
    }

    #[test]
    fn only_sound_files_are_written() {
        let e = output_path(Some(Path::new("hi.pdf")), "speech", None).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedPair);
        assert!(e.message.contains("m4a, mp3, wav or aiff"), "{}", e.message);
    }

    #[test]
    fn the_words_given_are_read() {
        let (source, out, note) = source(&args(Some("Hello there"), Some("hi.m4a"))).unwrap();
        assert_eq!(source, Source::Text("Hello there".to_string(), "the text"));
        assert_eq!(out, Some(PathBuf::from("hi.m4a")));
        assert_eq!(note, None);
    }

    #[test]
    fn a_file_named_as_text_is_read_as_text_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let typed = file.to_string_lossy().into_owned();
        let (source, _, note) = source(&args(Some(&typed), None)).unwrap();
        assert!(matches!(source, Source::Text(..)));
        assert!(note.unwrap().contains(&format!("-f {typed}")));
    }

    #[test]
    fn with_a_file_the_one_path_is_out() {
        let mut a = args(Some("out.wav"), None);
        a.file = Some(PathBuf::from("notes.md"));
        let (source, out, _) = source(&a).unwrap();
        assert_eq!(source, Source::File(PathBuf::from("notes.md")));
        assert_eq!(out, Some(PathBuf::from("out.wav")));
        a.output = Some(PathBuf::from("more.wav"));
        assert!(super::source(&a).is_err());
    }

    #[test]
    fn a_file_conv_cannot_read_aloud_is_refused() {
        let mut a = args(None, None);
        a.file = Some(PathBuf::from("photo.jpg"));
        assert_eq!(source(&a).unwrap_err().code, ErrorCode::UnsupportedPair);
    }

    #[test]
    fn blank_words_are_refused() {
        assert!(words("  \n".to_string()).is_err());
    }
}
