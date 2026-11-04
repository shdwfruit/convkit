//! Reading text aloud into a sound file, with the speech engine the system
//! already has: `say` on macOS, System.Speech through Windows PowerShell,
//! and espeak-ng on Linux. Piper, a neural engine, reads with one of its
//! voice files when `--voice` names one, on any of them.
//!
//! The engine writes AIFF or WAV, and ffmpeg makes the target from that
//! (`registry`). This module picks the engine and the voice, spells the
//! rate the way each engine takes it, and lists the voices.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::error::{ConvError, ErrorCode, Result};
use crate::procutil::backend_command;
use crate::{Backend, Resolver};

/// Words per minute `--rate` takes. espeak-ng reads no slower than 80 and
/// no faster than 450; say takes more either way, but neither end is
/// still speech anyone follows.
pub const RATE_MIN: u16 = 80;
pub const RATE_MAX: u16 = 450;
/// The rate every engine reads at by default, near enough: espeak-ng's
/// 175, say's 175 to 200 depending on the voice, Piper's and System.Speech's
/// own normal. The scale `--rate` is turned into for the two that take no
/// words per minute.
pub const NORMAL_RATE: u16 = 175;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Say,
    Sapi,
    Espeak,
    Piper,
}

impl Engine {
    /// The one this system reads with when no Piper voice is named.
    pub fn host() -> Engine {
        if cfg!(target_os = "macos") {
            Engine::Say
        } else if cfg!(windows) {
            Engine::Sapi
        } else {
            Engine::Espeak
        }
    }

    /// The engine for `--voice`: Piper for one of its voice files, the
    /// system's otherwise.
    pub fn for_voice(voice: Option<&str>) -> Engine {
        if voice.is_some_and(is_piper_voice) {
            Engine::Piper
        } else {
            Engine::host()
        }
    }

    pub fn all() -> [Engine; 4] {
        [Engine::Say, Engine::Sapi, Engine::Espeak, Engine::Piper]
    }

    pub fn backend(self) -> Backend {
        match self {
            Engine::Say => Backend::Say,
            Engine::Sapi => Backend::PowerShell,
            Engine::Espeak => Backend::Espeak,
            Engine::Piper => Backend::Piper,
        }
    }

    /// What this engine is called in a sentence.
    pub fn name(self) -> &'static str {
        match self {
            Engine::Say => "say",
            Engine::Sapi => "Windows speech",
            Engine::Espeak => "espeak-ng",
            Engine::Piper => "Piper",
        }
    }
}

/// A Piper voice is a file: `en_US-lessac-medium.onnx`, with its
/// `.onnx.json` beside it.
pub fn is_piper_voice(voice: &str) -> bool {
    Path::new(voice)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("onnx"))
}

/// How good a voice sounds, as macOS names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Standard,
    Enhanced,
    Premium,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Voice {
    /// What `--voice` takes.
    pub name: String,
    /// `en_US`, `en-us` or `en-US`, as the engine writes it.
    pub language: String,
    pub quality: Quality,
}

/// What was read before planning a conversion that speaks: the voices the
/// engine has and the user's language, for choosing one. Only say is read;
/// the others are left to choose their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpeechProbe {
    pub voices: Vec<Voice>,
    /// The user's first language, `en-US`.
    pub language: Option<String>,
    /// The Piper voice `--voice` names, or the config beside it, is not
    /// there.
    pub missing_voice_file: Option<PathBuf>,
}

/// Lists say's voices and the user's language, for the plan to choose
/// from, or checks that a Piper voice and its config are there. `None` for
/// any other engine, or when say could not be asked; the conversion then
/// reads with the system voice, or fails on a missing engine with its own
/// message.
pub fn read(resolver: &Resolver, engine: Engine, voice: Option<&str>) -> Option<SpeechProbe> {
    if engine == Engine::Piper {
        let model = PathBuf::from(voice?);
        let config = piper_config(&model);
        let missing = [model, config].into_iter().find(|p| !p.is_file());
        return Some(SpeechProbe {
            missing_voice_file: missing,
            ..SpeechProbe::default()
        });
    }
    if engine != Engine::Say {
        return None;
    }
    let say = resolver.resolve(Backend::Say).ok()?;
    let voices = say_voices(&say.path).ok()?;
    Some(SpeechProbe {
        voices,
        language: user_language(),
        missing_voice_file: None,
    })
}

/// The voices `engine` reads with, for `conv speak --voices`.
pub fn voices(resolver: &Resolver, engine: Engine) -> Result<Vec<Voice>> {
    let path = resolver.resolve(engine.backend())?.path;
    match engine {
        Engine::Say => say_voices(&path),
        Engine::Espeak => {
            let out = run(backend_command(&path).arg("--voices"), engine)?;
            Ok(parse_espeak(&out))
        }
        Engine::Sapi => {
            let script =
                std::env::temp_dir().join(format!("convkit-voices-{}.ps1", std::process::id()));
            std::fs::write(&script, SAPI_SCRIPT).map_err(|e| failed(engine, &e.to_string()))?;
            let mut cmd = backend_command(&path);
            cmd.args(SAPI_PREFIX).arg(&script).arg("-List");
            let listed = run(&mut cmd, engine);
            let _ = std::fs::remove_file(&script);
            Ok(parse_sapi(&listed?))
        }
        Engine::Piper => Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            "Piper's voices are files; download one from \
             https://huggingface.co/rhasspy/piper-voices and pass it as --voice VOICE.onnx",
        )),
    }
}

fn say_voices(say: &Path) -> Result<Vec<Voice>> {
    let mut cmd = backend_command(say);
    cmd.args(["-v", "?"]);
    Ok(parse_say(&run(&mut cmd, Engine::Say)?))
}

fn run(cmd: &mut Command, engine: Engine) -> Result<String> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| failed(engine, &e.to_string()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(failed(engine, err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn failed(engine: Engine, why: &str) -> ConvError {
    ConvError {
        code: ErrorCode::ConversionFailed,
        message: format!("could not list {}'s voices: {why}", engine.name()),
        backend: Some(engine.backend()),
        remediation: None,
    }
}

/// `say -v '?'`: a name, padded, the language, and `# ` and a sample
/// sentence. Names have spaces in them (`Bad News`, `Eddy (English (US))`,
/// `Ava (Premium)`), so the language is the last word before the `#`.
pub fn parse_say(listing: &str) -> Vec<Voice> {
    listing
        .lines()
        .filter_map(|line| {
            let (head, _) = line.split_once(" # ").or_else(|| line.split_once("\t#"))?;
            let head = head.trim_end();
            let (name, language) = head.rsplit_once(char::is_whitespace)?;
            let name = name.trim();
            if name.is_empty() || !language.contains('_') && !language.contains('-') {
                return None;
            }
            let quality = if name.ends_with("(Premium)") {
                Quality::Premium
            } else if name.ends_with("(Enhanced)") {
                Quality::Enhanced
            } else {
                Quality::Standard
            };
            Some(Voice {
                name: name.to_string(),
                language: language.to_string(),
                quality,
            })
        })
        .collect()
}

/// `espeak-ng --voices`: a header, then the priority, the language (what
/// `-v` takes), the age and gender, the voice's name and its file.
pub fn parse_espeak(listing: &str) -> Vec<Voice> {
    listing
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let _priority = cols.next()?;
            let language = cols.next()?;
            let _age_gender = cols.next()?;
            let name = cols.next().unwrap_or(language);
            Some(Voice {
                name: language.to_string(),
                language: name.replace('_', " "),
                quality: Quality::Standard,
            })
        })
        .collect()
}

/// conv's script with `-List`: a name and a culture a line, tab-separated.
pub fn parse_sapi(listing: &str) -> Vec<Voice> {
    listing
        .lines()
        .filter_map(|line| {
            let (name, language) = line.trim_end().split_once('\t')?;
            Some(Voice {
                name: name.to_string(),
                language: language.to_string(),
                quality: Quality::Standard,
            })
        })
        .collect()
}

/// The user's first language, for choosing a voice: the first of macOS's
/// preferred languages, or the locale the environment names.
fn user_language() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleLanguages"])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        let listed = String::from_utf8_lossy(&out.stdout);
        let first = listed
            .lines()
            .map(|l| l.trim().trim_matches(|c| c == '"' || c == ','))
            .find(|l| !l.is_empty() && *l != "(" && *l != ")");
        if let Some(lang) = first {
            return Some(lang.to_string());
        }
    }
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|v| v.split('.').next().unwrap_or_default().to_string())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
}

/// `en-US`, `en_US` or `en` as a language and a region, in lower case.
fn split_language(tag: &str) -> (String, Option<String>) {
    let mut parts = tag.split(['-', '_']);
    let lang = parts.next().unwrap_or_default().to_ascii_lowercase();
    // `zh-Hans-US`: the region is the last part, a script comes between.
    let region = parts.next_back().map(str::to_ascii_lowercase);
    (lang, region)
}

/// The voice a conversion reads with, and the note that says so when conv
/// chose it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chosen {
    /// What the engine's voice flag is given: `None` reads with the
    /// system's own voice.
    pub voice: Option<String>,
    pub note: Option<String>,
}

/// The note when say reads with the system voice because no better one is
/// installed.
pub const SYSTEM_VOICE_NOTE: &str = "Read with the system voice. A Premium or Enhanced voice \
     sounds far more natural: download one in System Settings > Accessibility > Spoken Content \
     > System voice > Manage Voices, and conv uses it.";

/// The note under `conv speak --voices` when none is Premium or Enhanced.
pub const NO_BETTER_VOICE_NOTE: &str = "None of these is a Premium or Enhanced voice, which \
     sound far more natural: download one in System Settings > Accessibility > Spoken Content \
     > System voice > Manage Voices.";

/// The note on espeak-ng's own voice.
pub const ESPEAK_NOTE: &str = "Read by espeak-ng, which sounds robotic. For a natural voice, \
     install Piper (pipx install piper-tts), download a voice from \
     https://huggingface.co/rhasspy/piper-voices and pass it: --voice en_US-lessac-medium.onnx.";

/// Picks the voice. `asked` is `--voice`: `system` for the system's own,
/// a Piper voice file, or a name. With say's voices read, a name is
/// checked against them, and `Ava` finds `Ava (Premium)`, the best of its
/// kind. With none asked, say reads with the best Premium or Enhanced
/// voice in the user's language, and otherwise with the system voice.
pub fn choose(engine: Engine, asked: Option<&str>, probe: Option<&SpeechProbe>) -> Result<Chosen> {
    if let Some(missing) = probe.and_then(|p| p.missing_voice_file.as_ref()) {
        return Err(ConvError {
            code: ErrorCode::InvalidInvocation,
            message: format!(
                "no Piper voice file at {}; a voice is an .onnx file and the .onnx.json \
                 beside it",
                missing.display()
            ),
            backend: None,
            remediation: Some(crate::error::Remediation {
                managed: None,
                manual: Some(
                    "download both from https://huggingface.co/rhasspy/piper-voices".to_string(),
                ),
            }),
        });
    }
    if let Some(asked) = asked {
        if asked.eq_ignore_ascii_case("system") {
            return Ok(Chosen::default());
        }
        let voice = match (engine, probe) {
            (Engine::Say, Some(p)) if !p.voices.is_empty() => find(&p.voices, asked)?,
            _ => asked.to_string(),
        };
        return Ok(Chosen {
            voice: Some(voice),
            note: None,
        });
    }
    match engine {
        Engine::Say => {
            let best = probe.and_then(|p| best_voice(&p.voices, p.language.as_deref()));
            Ok(match best {
                Some(v) => Chosen {
                    note: Some(format!(
                        "Read by {}; --voice picks another, and conv speak --voices lists them.",
                        v.name
                    )),
                    voice: Some(v.name.clone()),
                },
                None => Chosen {
                    voice: None,
                    note: Some(SYSTEM_VOICE_NOTE.to_string()),
                },
            })
        }
        Engine::Espeak => Ok(Chosen {
            voice: None,
            note: Some(ESPEAK_NOTE.to_string()),
        }),
        Engine::Sapi | Engine::Piper => Ok(Chosen::default()),
    }
}

/// The best Premium or Enhanced voice for `language`: the user's region
/// first, then any other of the same language. `None` without one.
fn best_voice<'a>(voices: &'a [Voice], language: Option<&str>) -> Option<&'a Voice> {
    let (lang, region) = split_language(language.unwrap_or("en-US"));
    voices
        .iter()
        .filter(|v| v.quality > Quality::Standard)
        .filter_map(|v| {
            let (l, r) = split_language(&v.language);
            (l == lang).then_some((v, r == region))
        })
        // Highest quality, then the user's region; the first listed wins a
        // tie, as `max_by_key` alone would give the last.
        .rev()
        .max_by_key(|(v, same_region)| (v.quality, *same_region))
        .map(|(v, _)| v)
}

/// The voice named `asked`, ignoring case: exactly, or by its name before
/// the brackets, the best of those.
fn find(voices: &[Voice], asked: &str) -> Result<String> {
    if let Some(v) = voices.iter().find(|v| v.name.eq_ignore_ascii_case(asked)) {
        return Ok(v.name.clone());
    }
    let bare = |name: &str| name.split(" (").next().unwrap_or(name).trim().to_string();
    let best = voices
        .iter()
        .filter(|v| bare(&v.name).eq_ignore_ascii_case(asked))
        .rev()
        .max_by_key(|v| v.quality);
    if let Some(v) = best {
        return Ok(v.name.clone());
    }
    let near = voices
        .iter()
        .map(|v| bare(&v.name))
        .min_by_key(|n| strsim::levenshtein(&n.to_lowercase(), &asked.to_lowercase()))
        .filter(|n| strsim::levenshtein(&n.to_lowercase(), &asked.to_lowercase()) <= 2);
    let message = match near {
        Some(n) => format!("no voice named {asked:?}; did you mean {n:?}?"),
        None => format!("no voice named {asked:?}"),
    };
    Err(ConvError {
        code: ErrorCode::InvalidInvocation,
        message,
        backend: None,
        remediation: Some(crate::error::Remediation {
            managed: Some("conv speak --voices".to_string()),
            manual: None,
        }),
    })
}

/// `--rate` for System.Speech, whose rate runs from -10 to 10 with 0 its
/// normal. Each end is about three times slower or faster than normal, so
/// a step is a tenth of a power of three.
pub fn sapi_rate(wpm: u16) -> i32 {
    let steps = 10.0 * (f64::from(wpm) / f64::from(NORMAL_RATE)).ln() / 3f64.ln();
    (steps.round() as i32).clamp(-10, 10)
}

/// `--rate` for Piper, whose `--length_scale` stretches every sound: 2 is
/// half as fast.
pub fn piper_length_scale(wpm: u16) -> String {
    format!("{:.2}", f64::from(NORMAL_RATE) / f64::from(wpm))
}

/// What precedes the script in a Windows PowerShell command line. The
/// script runs from a file so its paths are arguments of their own;
/// `Bypass` holds for this one process, so a machine set to run no
/// scripts still runs it, unless its administrator has forbidden that.
pub const SAPI_PREFIX: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
];

/// conv's System.Speech script: reads `-In`, UTF-8 text, into `-Out`, a
/// WAV file, with `-Voice` (its name, or a part of it that only one voice
/// has) at `-Rate`. `-List` prints the voices instead. Every failure is
/// one line on stderr and exit 1, which conv reports.
pub const SAPI_SCRIPT: &str = r#"# Written by conv: reads a text file aloud into a WAV file with
# System.Speech, which every Windows has.
param([string]$In, [string]$Out, [string]$Voice, [int]$Rate = 0, [switch]$List)
$ErrorActionPreference = 'Stop'
try {
  Add-Type -AssemblyName System.Speech
  $s = New-Object System.Speech.Synthesis.SpeechSynthesizer
  $names = @($s.GetInstalledVoices() | Where-Object { $_.Enabled } | ForEach-Object { $_.VoiceInfo.Name })
  if ($List) {
    foreach ($v in $s.GetInstalledVoices()) {
      if ($v.Enabled) { [Console]::Out.WriteLine($v.VoiceInfo.Name + "`t" + $v.VoiceInfo.Culture.Name) }
    }
    exit 0
  }
  if ($Voice) {
    $match = @($names | Where-Object { $_ -eq $Voice })
    if ($match.Count -eq 0) { $match = @($names | Where-Object { $_ -like "*$Voice*" }) }
    if ($match.Count -ne 1) {
      [Console]::Error.WriteLine("no voice named $Voice; conv speak --voices lists them")
      exit 1
    }
    $s.SelectVoice($match[0])
  }
  $s.Rate = $Rate
  $s.SetOutputToWaveFile($Out)
  $s.Speak([IO.File]::ReadAllText($In, [Text.Encoding]::UTF8))
  $s.Dispose()
} catch {
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}
"#;

/// Where a Piper voice's config is looked for, to say it is missing before
/// Piper fails on it less clearly.
pub fn piper_config(voice: &Path) -> PathBuf {
    let mut name = voice.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAY_LISTING: &str = "\
Albert              en_US    # Hello! My name is Albert.
Bad News            en_US    # Hello! My name is Bad News.
Daniel              en_GB    # Hello! My name is Daniel.
Eddy (English (US)) en_US    # Hello! My name is Eddy.
Ava (Enhanced)      en_US    # Hello! My name is Ava.
Ava (Premium)       en_US    # Hello! My name is Ava.
Serena (Premium)    en_GB    # Hello! My name is Serena.
Anna (Enhanced)     de_DE    # Hallo! Ich heiße Anna.
Samantha            en_US    # Hello! My name is Samantha.
";

    fn probe(language: &str) -> SpeechProbe {
        SpeechProbe {
            voices: parse_say(SAY_LISTING),
            language: Some(language.to_string()),
            missing_voice_file: None,
        }
    }

    #[test]
    fn a_missing_piper_voice_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("en.onnx");
        std::fs::write(&model, b"model").unwrap();
        let voice = model.to_string_lossy().into_owned();
        let p = read(&Resolver::new(), Engine::Piper, Some(&voice)).unwrap();
        assert_eq!(p.missing_voice_file, Some(piper_config(&model)));
        let e = choose(Engine::Piper, Some(&voice), Some(&p)).unwrap_err();
        assert!(e.message.contains("en.onnx.json"), "{}", e.message);
        std::fs::write(piper_config(&model), b"{}").unwrap();
        let p = read(&Resolver::new(), Engine::Piper, Some(&voice)).unwrap();
        assert_eq!(p.missing_voice_file, None);
        assert_eq!(
            choose(Engine::Piper, Some(&voice), Some(&p)).unwrap().voice,
            Some(voice)
        );
    }

    #[test]
    fn say_listing_keeps_names_with_spaces_and_brackets() {
        let voices = parse_say(SAY_LISTING);
        assert_eq!(voices.len(), 9);
        assert_eq!(voices[1].name, "Bad News");
        assert_eq!(voices[3].name, "Eddy (English (US))");
        assert_eq!(voices[3].language, "en_US");
        assert_eq!(voices[5].quality, Quality::Premium);
        assert_eq!(voices[4].quality, Quality::Enhanced);
        assert_eq!(voices[0].quality, Quality::Standard);
    }

    #[test]
    fn a_name_too_long_for_its_column_still_parses() {
        let voices =
            parse_say("Grandpa (Portuguese (Brazil)) pt_BR    # Olá, meu nome é Grandpa.\n");
        assert_eq!(voices[0].name, "Grandpa (Portuguese (Brazil))");
        assert_eq!(voices[0].language, "pt_BR");
    }

    #[test]
    fn the_best_voice_in_the_users_language_is_chosen() {
        let c = choose(Engine::Say, None, Some(&probe("en-US"))).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Ava (Premium)"));
        assert!(c.note.unwrap().starts_with("Read by Ava (Premium);"));
        // The region decides between two Premium voices.
        let c = choose(Engine::Say, None, Some(&probe("en-GB"))).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Serena (Premium)"));
        let c = choose(Engine::Say, None, Some(&probe("de-DE"))).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Anna (Enhanced)"));
    }

    #[test]
    fn without_a_better_voice_the_system_voice_reads_and_the_note_says_how_to_get_one() {
        let c = choose(Engine::Say, None, Some(&probe("fr-FR"))).unwrap();
        assert_eq!(c.voice, None);
        assert_eq!(c.note.as_deref(), Some(SYSTEM_VOICE_NOTE));
        // And when the voices could not be listed at all.
        let c = choose(Engine::Say, None, None).unwrap();
        assert_eq!(c.voice, None);
    }

    #[test]
    fn a_named_voice_is_matched_by_its_bare_name_and_quality() {
        let p = probe("en-US");
        let c = choose(Engine::Say, Some("ava"), Some(&p)).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Ava (Premium)"));
        assert_eq!(c.note, None);
        let c = choose(Engine::Say, Some("ava (enhanced)"), Some(&p)).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Ava (Enhanced)"));
        let c = choose(Engine::Say, Some("bad news"), Some(&p)).unwrap();
        assert_eq!(c.voice.as_deref(), Some("Bad News"));
    }

    #[test]
    fn an_unknown_voice_is_refused_with_a_near_miss() {
        let e = choose(Engine::Say, Some("Samanta"), Some(&probe("en-US"))).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "no voice named \"Samanta\"; did you mean \"Samantha\"?"
        );
        assert_eq!(
            e.remediation.unwrap().managed.as_deref(),
            Some("conv speak --voices")
        );
    }

    #[test]
    fn system_reads_with_the_system_voice() {
        let c = choose(Engine::Say, Some("System"), Some(&probe("en-US"))).unwrap();
        assert_eq!(c, Chosen::default());
    }

    #[test]
    fn other_engines_take_the_name_as_given() {
        let c = choose(Engine::Espeak, Some("de"), None).unwrap();
        assert_eq!(c.voice.as_deref(), Some("de"));
        let c = choose(Engine::Espeak, None, None).unwrap();
        assert_eq!(c.note.as_deref(), Some(ESPEAK_NOTE));
        assert_eq!(choose(Engine::Sapi, None, None).unwrap(), Chosen::default());
    }

    #[test]
    fn a_piper_voice_file_picks_piper() {
        assert_eq!(
            Engine::for_voice(Some("voices/en_US-lessac-medium.onnx")),
            Engine::Piper
        );
        assert_eq!(Engine::for_voice(Some("Ava")), Engine::host());
        assert_eq!(Engine::for_voice(None), Engine::host());
        assert_eq!(
            piper_config(Path::new("v/en.onnx")),
            PathBuf::from("v/en.onnx.json")
        );
    }

    #[test]
    fn rates_are_spelled_for_each_engine() {
        assert_eq!(sapi_rate(NORMAL_RATE), 0);
        assert_eq!(sapi_rate(525), 10);
        assert_eq!(sapi_rate(RATE_MIN), -7);
        assert_eq!(sapi_rate(RATE_MAX), 9);
        assert_eq!(piper_length_scale(NORMAL_RATE), "1.00");
        assert_eq!(piper_length_scale(350), "0.50");
    }

    #[test]
    fn espeak_and_sapi_listings_parse() {
        let espeak = "\
Pty Language       Age/Gender VoiceName          File                 Other Languages
 5  af              --/M      Afrikaans          gmw/af
 2  en-us           --/M      English_(America)  gmw/en-US            (en 3)
";
        let v = parse_espeak(espeak);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].name, "en-us");
        assert_eq!(v[1].language, "English (America)");
        let sapi = "Microsoft David Desktop\ten-US\r\nMicrosoft Zira Desktop\ten-US\r\n";
        let v = parse_sapi(sapi);
        assert_eq!(v[1].name, "Microsoft Zira Desktop");
        assert_eq!(v[1].language, "en-US");
    }
}
