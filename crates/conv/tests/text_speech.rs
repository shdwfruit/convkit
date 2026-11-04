//! The command line for text, HTML, RTF and AIFF, and for text read aloud
//! (`conv notes.txt .m4a`, `conv speak`). Previews need no backend; the
//! `#[ignore]` ones at the end run the real ones.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn conv() -> Command {
    Command::cargo_bin("conv").unwrap()
}

/// `conv` with no backend anywhere it looks, as on a bare CI runner.
fn bare() -> (Command, tempfile::TempDir, tempfile::TempDir) {
    let empty_path = tempfile::tempdir().unwrap();
    let empty_managed = tempfile::tempdir().unwrap();
    let mut cmd = conv();
    cmd.env_clear();
    if let Ok(root) = std::env::var("SYSTEMROOT") {
        cmd.env("SYSTEMROOT", root);
    }
    cmd.env("PATH", empty_path.path());
    #[cfg(windows)]
    cmd.env("LOCALAPPDATA", empty_managed.path());
    #[cfg(not(windows))]
    cmd.env("XDG_DATA_HOME", empty_managed.path());
    cmd.env("CONVKIT_NO_WELL_KNOWN", "1");
    (cmd, empty_path, empty_managed)
}

fn stub(dir: &Path, name: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, b"stub").unwrap();
    p
}

fn json(out: &[u8]) -> serde_json::Value {
    serde_json::from_slice(out)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(out)))
}

#[test]
fn txt_to_pdf_plans_pandoc_then_libreoffice() {
    let dir = tempfile::tempdir().unwrap();
    let soffice = stub(dir.path(), "soffice_stub");
    conv()
        .current_dir(dir.path())
        .args(["notes.txt", "notes.pdf", "--dry-run", "--soffice-path"])
        .arg(&soffice)
        .assert()
        .success()
        .stdout(contains(
            "pandoc notes.convkit-text.html -f html --standalone -o notes.convkit-step0.docx",
        ))
        .stdout(contains(
            "--convert-to pdf --outdir . notes.convkit-step0.docx",
        ));
}

#[test]
fn html_to_pdf_plans_an_offline_pandoc_then_libreoffice() {
    let dir = tempfile::tempdir().unwrap();
    let soffice = stub(dir.path(), "soffice_stub");
    let assert = conv()
        .current_dir(dir.path())
        .args([
            "page.html",
            "page.pdf",
            "--dry-run",
            "--json",
            "--soffice-path",
        ])
        .arg(&soffice)
        .assert()
        .success();
    let v = json(&assert.get_output().stdout);
    let steps = &v["plans"][0]["plan"]["steps"];
    assert_eq!(steps[0]["program"], "pandoc");
    assert_eq!(steps[1]["program"], "soffice");
    let argv: Vec<&str> = steps[0]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(
        argv.windows(2).any(|w| w == ["-f", "html+raw_html"]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w == ["--lua-filter", "page.convkit-offline.lua"]),
        "{argv:?}"
    );
    assert_eq!(steps[0]["prepare"][0]["kind"], "write");
}

#[test]
fn rtf_plans_libreoffice_and_falls_back_to_pandoc_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let soffice = stub(dir.path(), "soffice_stub");
    let pandoc = stub(dir.path(), "pandoc_stub");
    let typst = stub(dir.path(), "typst_stub");
    for to in ["letter.docx", "letter.txt", "letter.pdf"] {
        conv()
            .current_dir(dir.path())
            .args(["letter.rtf", to, "--dry-run", "--soffice-path"])
            .arg(&soffice)
            .assert()
            .success()
            .stdout(contains("soffice"))
            .stdout(contains("pandoc").not());
    }
    let (mut cmd, _p, _m) = bare();
    cmd.current_dir(dir.path())
        .args(["letter.rtf", "letter.docx", "--dry-run", "--pandoc-path"])
        .arg(&pandoc)
        .assert()
        .success()
        .stdout(contains("pandoc letter.rtf --standalone"))
        .stdout(contains(
            "pandoc read the RTF, as LibreOffice isn't installed",
        ));
    let (mut cmd, _p, _m) = bare();
    cmd.current_dir(dir.path())
        .args(["letter.rtf", "letter.pdf", "--dry-run", "--pandoc-path"])
        .arg(&pandoc)
        .arg("--typst-path")
        .arg(&typst)
        .assert()
        .success()
        .stdout(contains("--pdf-engine"));
}

#[test]
fn documents_become_txt_and_rtf_through_pandoc() {
    for (input, output, wants) in [
        ("a.docx", "a.txt", "-t plain '--wrap=none'"),
        ("a.odt", "a.txt", "-t plain"),
        ("a.md", "a.txt", "-t plain"),
        ("a.html", "a.txt", "-f html+raw_html -t plain"),
        ("a.html", "a.md", "-t gfm-raw_html"),
        (
            "a.docx",
            "a.rtf",
            "--lua-filter a.convkit-offline.lua -o a.rtf",
        ),
        ("a.md", "a.rtf", "--lua-filter"),
        ("a.txt", "a.md", "a.convkit-text.html -f html -t gfm"),
    ] {
        conv()
            .args([input, output, "--dry-run"])
            .assert()
            .success()
            .stdout(contains("pandoc ").and(contains(wants)));
    }
}

#[test]
fn a_txt_that_is_not_utf8_is_noted_and_a_binary_one_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("latin.txt"), b"Caf\xe9 na\xefve\n").unwrap();
    std::fs::write(
        dir.path().join("logo.txt"),
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR",
    )
    .unwrap();
    conv()
        .current_dir(dir.path())
        .args(["latin.txt", ".md", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("read as Windows-1252"));
    conv()
        .current_dir(dir.path())
        .args(["logo.txt", ".md", "--dry-run"])
        .assert()
        .code(1)
        .stderr(contains("logo.txt is not text"));
}

#[test]
fn aiff_is_written_and_read_by_ffmpeg() {
    conv()
        .args(["song.mp3", "song.aiff", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("-c:a pcm_s16be"));
    conv()
        .args(["take.aif", ".m4a", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("ffmpeg -i take.aif"));
}

#[test]
fn text_read_aloud_plans_the_systems_engine_then_ffmpeg() {
    let (mut cmd, _p, _m) = bare();
    let dir = tempfile::tempdir().unwrap();
    let assert = cmd
        .current_dir(dir.path())
        .args(["notes.txt", ".m4a", "--dry-run", "--json"])
        .assert()
        .success();
    let v = json(&assert.get_output().stdout);
    let plan = &v["plans"][0]["plan"];
    let engine = if cfg!(target_os = "macos") {
        "say"
    } else if cfg!(windows) {
        "powershell"
    } else {
        "espeak-ng"
    };
    assert_eq!(plan["steps"][0]["program"], engine);
    assert_eq!(plan["steps"][1]["program"], "ffmpeg");
    assert_eq!(plan["output"], "notes.m4a");
    assert_eq!(plan["steps"][0]["prepare"][0]["form"], "speech");
}

#[test]
fn a_document_is_reduced_to_text_before_it_is_read() {
    conv()
        .args(["report.docx", "report.mp3", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("pandoc report.docx -t plain"))
        .stdout(contains("-c:a libmp3lame"));
}

#[test]
fn a_piper_voice_reads_with_piper_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    conv()
        .current_dir(dir.path())
        .args([
            "notes.md",
            ".wav",
            "--voice",
            "en_US-lessac-medium.onnx",
            "--dry-run",
        ])
        .assert()
        .code(2)
        .stderr(contains("no Piper voice file at en_US-lessac-medium.onnx"));
    std::fs::write(dir.path().join("en_US-lessac-medium.onnx"), b"model").unwrap();
    std::fs::write(dir.path().join("en_US-lessac-medium.onnx.json"), b"{}").unwrap();
    let assert = conv()
        .current_dir(dir.path())
        .args([
            "notes.md",
            ".wav",
            "--voice",
            "en_US-lessac-medium.onnx",
            "--rate",
            "200",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success();
    let v = json(&assert.get_output().stdout);
    let step = &v["plans"][0]["plan"]["steps"][1];
    assert_eq!(step["program"], "piper");
    assert_eq!(step["stdin"], "notes.convkit-speech.txt");
    assert_eq!(step["argv"][0], "--model");
    assert_eq!(step["argv"][3], "0.88");
}

#[test]
fn voice_and_rate_only_apply_to_text_read_aloud() {
    conv()
        .args(["a.png", "a.jpg", "--voice", "Ava", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("--voice does not apply to png -> jpg"));
    conv()
        .args(["a.txt", "a.mp3", "--start", "3", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("text read aloud takes --voice and --rate"));
    conv()
        .args(["a.txt", "a.mp3", "--rate", "30", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("give 80 to 450"));
}

#[test]
fn capabilities_lists_the_speech_flags() {
    conv()
        .args(["capabilities", "txt"])
        .assert()
        .success()
        .stdout(contains("txt -> m4a      [--voice --rate]"));
}

#[test]
fn speak_reads_the_words_given_into_speech_mp3() {
    let (mut cmd, _p, _m) = bare();
    let dir = tempfile::tempdir().unwrap();
    cmd.current_dir(dir.path())
        .args(["speak", "Hello there", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("-y speech.mp3"));
    let (mut cmd, _p, _m) = bare();
    cmd.current_dir(dir.path())
        .args(["speak", "Hello", ".wav", "-o", "out", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("speech.wav"));
    assert!(
        !dir.path().join("out").exists(),
        "a preview makes no folder"
    );
}

#[test]
fn speak_reads_a_pipe_and_a_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("story.md"), "# A story\n\nOnce.\n").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["speak", "-", "piped.m4a", "--dry-run"])
        .write_stdin("words from a pipe")
        .assert()
        .success()
        .stdout(contains("piped.m4a"));
    conv()
        .current_dir(dir.path())
        .args(["speak", "-f", "story.md", ".aiff", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("pandoc story.md -t plain"))
        .stdout(contains("story.aiff"));
}

#[test]
fn speak_says_what_it_needs() {
    conv()
        .args(["speak"])
        .write_stdin("   \n")
        .assert()
        .code(2)
        .stderr(contains("there is no text to read aloud"));
    conv()
        .args(["speak", "Hello", "hello.pdf"])
        .assert()
        .code(2)
        .stderr(contains("m4a, mp3, wav or aiff"));
    conv()
        .args(["speak", "-f", "photo.jpg"])
        .assert()
        .code(2)
        .stderr(contains("txt, md, html, docx, odt and rtf"));
}

#[test]
fn speak_notes_a_file_given_as_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["speak", "notes.txt", "--dry-run"])
        .assert()
        .success()
        .stderr(contains("-f notes.txt reads the file"));
}

#[test]
fn a_missing_speech_engine_says_how_to_get_one() {
    if !cfg!(target_os = "linux") {
        return; // say and PowerShell come with their systems.
    }
    let (mut cmd, _p, _m) = bare();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "Hello").unwrap();
    cmd.current_dir(dir.path())
        .args(["a.txt", "a.wav"])
        .assert()
        .code(3)
        .stderr(contains("espeak-ng not found"));
}

#[test]
#[ignore = "requires the system's speech engine and ffmpeg; run with --ignored"]
fn speak_writes_a_sound_file_for_real() {
    let dir = tempfile::tempdir().unwrap();
    let assert = conv()
        .current_dir(dir.path())
        .args([
            "speak",
            "Hello from conv, reading aloud.",
            "hello.m4a",
            "--json",
        ])
        .assert()
        .success();
    let v = json(&assert.get_output().stdout);
    assert_eq!(v["results"][0]["input"], "the text");
    let bytes = std::fs::metadata(dir.path().join("hello.m4a"))
        .unwrap()
        .len();
    assert!(bytes > 1000, "{bytes} bytes");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, ["hello.m4a"], "nothing else is left behind");
}
