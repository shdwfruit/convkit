//! Text, HTML and RTF documents, AIFF, and text read aloud.
//!
//! The first half plans conversions with no backend at all, as the recipe
//! snapshot does. The second half (`#[ignore]`) runs them for real and
//! checks what comes out: the text, the codec, and a length that fits the
//! words. Text is read aloud by the system's own engine, so those tests
//! need say (macOS), espeak-ng (Linux) or Windows PowerShell, and ffmpeg.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use convkit_core::plan::{self, Prepare, TextForm};
use convkit_core::speech::{Engine, SpeechProbe};
use convkit_core::text::{Encoding, TextInfo};
use convkit_core::{
    exec, registry, AvailableBackends, Backend, ErrorCode, Format, MediaProbe, Resolver, Tuning,
};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn speech_tuning(voice: Option<&str>, rate: Option<u16>) -> Tuning {
    Tuning {
        voice: voice.map(str::to_string),
        rate,
        ..Tuning::default()
    }
}

// --- Planning --------------------------------------------------------------

#[test]
fn a_txt_source_is_read_by_conv_and_handed_to_pandoc_as_a_page() {
    let plan = plan::build(
        Format::Txt,
        Format::Docx,
        &[p("notes/todo.txt")],
        Path::new("out/todo.docx"),
        None,
        None,
    )
    .unwrap();
    let step = &plan.steps[0];
    assert_eq!(step.program, "pandoc");
    // Compared as paths: Windows writes it with a backslash.
    assert_eq!(
        Path::new(&step.argv[0]),
        Path::new("out/todo.convkit-text.html")
    );
    assert_eq!(
        step.prepare,
        vec![Prepare::Text {
            from: p("notes/todo.txt"),
            to: p("out/todo.convkit-text.html"),
            form: TextForm::Page {
                title: "todo".to_string()
            },
            source: p("notes/todo.txt"),
        }]
    );
}

#[test]
fn a_txt_in_windows_1252_says_so_and_a_binary_one_is_refused() {
    let read = |r: Result<TextInfo, String>| MediaProbe {
        text: Some(r),
        ..MediaProbe::default()
    };
    let latin = read(Ok(TextInfo {
        encoding: Encoding::Windows1252,
        empty: false,
    }));
    let plan = plan::build(
        Format::Txt,
        Format::Md,
        &[p("a.txt")],
        Path::new("a.md"),
        Some(&latin),
        None,
    )
    .unwrap();
    assert_eq!(plan.warnings, vec![convkit_core::text::WINDOWS_1252_NOTE]);
    let utf16 = read(Ok(TextInfo {
        encoding: Encoding::Utf16Le,
        empty: false,
    }));
    let plan = plan::build(
        Format::Txt,
        Format::Md,
        &[p("a.txt")],
        Path::new("a.md"),
        Some(&utf16),
        None,
    )
    .unwrap();
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    let binary = read(Err("it holds zero bytes, as binary files do".to_string()));
    let e = plan::build(
        Format::Txt,
        Format::Pdf,
        &[p("dir/a.txt")],
        Path::new("a.pdf"),
        Some(&binary),
        None,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ConversionFailed);
    assert_eq!(
        e.message,
        "a.txt is not text: it holds zero bytes, as binary files do"
    );
}

#[test]
fn txt_and_html_to_pdf_go_through_docx_and_libreoffice_as_markdown_does() {
    for from in [Format::Txt, Format::Html, Format::Md] {
        let input = p(&format!("in.{}", from.ext()));
        let plan = plan::build(
            from,
            Format::Pdf,
            &[input],
            Path::new("out.pdf"),
            None,
            None,
        )
        .unwrap();
        let programs: Vec<&str> = plan.steps.iter().map(|s| s.program.as_str()).collect();
        assert_eq!(programs, ["pandoc", "soffice"], "{from:?}");
        assert!(plan.steps[0]
            .argv
            .contains(&"out.convkit-step0.docx".to_string()));
    }
}

#[test]
fn html_is_read_without_fetching_and_written_without_downloading_pictures() {
    for to in [Format::Docx, Format::Rtf, Format::Pdf, Format::Md] {
        let plan = plan::build(
            Format::Html,
            to,
            &[p("page.html")],
            Path::new("out.x"),
            None,
            None,
        )
        .unwrap();
        let step = &plan.steps[0];
        let argv = step.argv.join(" ");
        assert!(argv.contains("-f html+raw_html"), "{to:?}: {argv}");
        assert!(
            argv.contains("--lua-filter out.convkit-offline.lua"),
            "{to:?}: {argv}"
        );
        assert!(
            step.prepare.iter().any(|p| matches!(
                p,
                Prepare::Write { path, contents }
                    if path == Path::new("out.convkit-offline.lua")
                        && contents == convkit_core::offline::FILTER
            )),
            "{to:?}: {:?}",
            step.prepare
        );
    }
    let plan = plan::build(
        Format::Html,
        Format::Txt,
        &[p("page.html")],
        Path::new("out.txt"),
        None,
        None,
    )
    .unwrap();
    assert!(plan.steps[0].argv.join(" ").contains("-f html+raw_html"));
}

#[test]
fn rtf_is_read_by_libreoffice_and_by_pandoc_only_without_it() {
    let without: AvailableBackends = [Backend::Pandoc].into_iter().collect();
    let with: AvailableBackends = [Backend::Soffice, Backend::Pandoc].into_iter().collect();
    for to in [Format::Docx, Format::Txt] {
        let input = [p("in.rtf")];
        let out = p(&format!("out.{}", to.ext()));
        let plan = plan::build(Format::Rtf, to, &input, &out, None, Some(&with)).unwrap();
        assert_eq!(plan.steps[0].program, "soffice", "{to:?}");
        assert!(plan.warnings.is_empty());
        let plan = plan::build(Format::Rtf, to, &input, &out, None, Some(&without)).unwrap();
        assert_eq!(plan.steps[0].program, "pandoc", "{to:?}");
        assert!(plan.warnings[0].starts_with("pandoc read the RTF"));
    }
    // To PDF, pandoc needs Typst too.
    let input = [p("in.rtf")];
    let plan = plan::build(
        Format::Rtf,
        Format::Pdf,
        &input,
        Path::new("o.pdf"),
        None,
        Some(&without),
    )
    .unwrap();
    assert_eq!(plan.steps[0].program, "soffice");
    let both: AvailableBackends = [Backend::Pandoc, Backend::Typst].into_iter().collect();
    let plan = plan::build(
        Format::Rtf,
        Format::Pdf,
        &input,
        Path::new("o.pdf"),
        None,
        Some(&both),
    )
    .unwrap();
    assert!(plan.steps[0].argv.contains(&"--pdf-engine".to_string()));
    assert_eq!(
        registry::fallback_backends(Format::Rtf, Format::Mp3),
        [Backend::Soffice, Backend::Pandoc]
    );
}

#[test]
fn aiff_is_an_audio_format_in_both_directions() {
    let pairs = registry::all_pairs();
    for other in [
        Format::Mp3,
        Format::M4a,
        Format::Wav,
        Format::Flac,
        Format::Ogg,
    ] {
        assert!(pairs.contains(&(Format::Aiff, other)), "aiff -> {other:?}");
        assert!(pairs.contains(&(other, Format::Aiff)), "{other:?} -> aiff");
    }
    assert!(pairs.contains(&(Format::Mov, Format::Aiff)));
    assert_eq!(Format::from_ext("aif"), Some(Format::Aiff));
    let plan = plan::build(
        Format::Wav,
        Format::Aiff,
        &[p("a.wav")],
        Path::new("a.aiff"),
        None,
        None,
    )
    .unwrap();
    assert!(plan.steps[0].argv.contains(&"pcm_s16be".to_string()));
    // It cuts as the other audio targets do.
    assert!(registry::takes_range(Format::Mp3, Format::Aiff));
    assert!(registry::takes_range(Format::Aiff, Format::Aiff));
}

#[test]
fn text_is_read_aloud_by_the_systems_engine_into_every_sound_target() {
    for from in [
        Format::Txt,
        Format::Md,
        Format::Html,
        Format::Docx,
        Format::Odt,
        Format::Rtf,
    ] {
        for to in [Format::M4a, Format::Mp3, Format::Wav, Format::Aiff] {
            assert!(registry::speaks(from, to), "{from:?} -> {to:?}");
            let input = p(&format!("in.{}", from.ext()));
            let out = p(&format!("out.{}", to.ext()));
            let plan = plan::build(from, to, &[input], &out, None, None).unwrap();
            let engine = plan.steps.iter().find(|s| s.backend.speaks()).unwrap();
            assert_eq!(engine.backend, Engine::host().backend());
            let last = plan.steps.last().unwrap();
            assert_eq!(last.program, "ffmpeg");
            assert_eq!(last.output, out);
            // A document is reduced to text first; a .txt is read by conv.
            assert_eq!(plan.steps.len(), if from == Format::Txt { 2 } else { 3 });
            assert!(engine.prepare.iter().any(|p| matches!(
                p,
                Prepare::Text {
                    form: TextForm::Speech,
                    ..
                }
            )));
        }
    }
    assert!(!registry::speaks(Format::Pdf, Format::Mp3));
    assert!(!registry::speaks(Format::Txt, Format::Flac));
}

#[test]
fn every_engine_takes_the_voice_and_the_rate_in_its_own_spelling() {
    let tuning = speech_tuning(Some("Ava"), Some(350));
    let argv = |engine| {
        let plan = plan::build_speech(
            engine,
            Format::Txt,
            Format::Wav,
            &[p("in.txt")],
            Path::new("out.wav"),
            None,
            None,
            &tuning,
        )
        .unwrap();
        plan.steps[0].argv.join(" ")
    };
    assert!(argv(Engine::Say).starts_with("-v Ava -r 350 -o "));
    assert!(argv(Engine::Espeak).starts_with("-v Ava -s 350 -b 1 -w "));
    assert!(
        argv(Engine::Sapi).ends_with("-Voice Ava -Rate 6"),
        "{}",
        argv(Engine::Sapi)
    );
    let piper = speech_tuning(Some("v/en.onnx"), Some(350));
    let plan = plan::build_speech(
        Engine::Piper,
        Format::Txt,
        Format::Wav,
        &[p("in.txt")],
        Path::new("out.wav"),
        None,
        None,
        &piper,
    )
    .unwrap();
    let step = &plan.steps[0];
    assert_eq!(
        step.argv.join(" "),
        "--model v/en.onnx --length_scale 0.50 --output_file out.convkit-step0.wav"
    );
    assert_eq!(step.stdin, Some(p("out.convkit-speech.txt")));
}

#[test]
fn a_piper_voice_file_reads_with_piper() {
    let tuning = speech_tuning(Some("voices/en_US-lessac-medium.onnx"), None);
    let plan = plan::build_tuned(
        Format::Md,
        Format::Mp3,
        &[p("in.md")],
        Path::new("out.mp3"),
        None,
        None,
        &tuning,
    )
    .unwrap();
    assert_eq!(plan.steps[1].program, "piper");
}

#[test]
fn say_reads_with_the_best_voice_it_has_and_says_which() {
    let probe = MediaProbe {
        speech: Some(SpeechProbe {
            voices: convkit_core::speech::parse_say(
                "Albert              en_US    # Hello! My name is Albert.\n\
                 Zoe (Premium)       en_US    # Hello! My name is Zoe.\n",
            ),
            language: Some("en-US".to_string()),
            missing_voice_file: None,
        }),
        ..MediaProbe::default()
    };
    let plan = plan::build_speech(
        Engine::Say,
        Format::Txt,
        Format::M4a,
        &[p("in.txt")],
        Path::new("out.m4a"),
        Some(&probe),
        None,
        &Tuning::default(),
    )
    .unwrap();
    assert_eq!(plan.steps[0].argv[..2], ["-v", "Zoe (Premium)"]);
    assert!(plan.warnings[0].starts_with("Read by Zoe (Premium)"));
    let e = plan::build_speech(
        Engine::Say,
        Format::Txt,
        Format::M4a,
        &[p("in.txt")],
        Path::new("out.m4a"),
        Some(&probe),
        None,
        &speech_tuning(Some("Nobody"), None),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInvocation);
}

#[test]
fn voice_and_rate_are_refused_where_nothing_is_read_aloud() {
    let e = plan::build_tuned(
        Format::Png,
        Format::Jpg,
        &[p("a.png")],
        Path::new("a.jpg"),
        None,
        None,
        &speech_tuning(Some("Ava"), None),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInvocation);
    assert!(e
        .message
        .starts_with("--voice does not apply to png -> jpg"));
    let e = plan::build_tuned(
        Format::Mp3,
        Format::Wav,
        &[p("a.mp3")],
        Path::new("a.wav"),
        None,
        None,
        &speech_tuning(None, Some(200)),
    )
    .unwrap_err();
    assert!(
        e.message.starts_with("--rate does not apply"),
        "{}",
        e.message
    );
}

#[test]
fn text_read_aloud_takes_no_other_knob() {
    for tuning in [
        Tuning {
            strip_metadata: true,
            ..Tuning::default()
        },
        Tuning {
            speed: Some(convkit_core::speed::parse("2").unwrap()),
            ..Tuning::default()
        },
        Tuning {
            quality: Some(50),
            ..Tuning::default()
        },
    ] {
        let e = plan::build_tuned(
            Format::Txt,
            Format::Mp3,
            &[p("a.txt")],
            Path::new("a.mp3"),
            None,
            None,
            &tuning,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert!(
            e.message
                .ends_with("text read aloud takes --voice and --rate"),
            "{}",
            e.message
        );
    }
}

#[test]
fn an_empty_txt_is_not_read_aloud() {
    let empty = MediaProbe {
        text: Some(Ok(TextInfo {
            encoding: Encoding::Utf8,
            empty: true,
        })),
        ..MediaProbe::default()
    };
    let e = plan::build(
        Format::Txt,
        Format::Wav,
        &[p("blank.txt")],
        Path::new("blank.wav"),
        Some(&empty),
        None,
    )
    .unwrap_err();
    assert_eq!(e.message, "blank.txt has no text to read aloud");
    // Converting it to a document is harmless, and allowed.
    assert!(plan::build(
        Format::Txt,
        Format::Docx,
        &[p("blank.txt")],
        Path::new("blank.docx"),
        Some(&empty),
        None,
    )
    .is_ok());
}

// --- Real backends -----------------------------------------------------------

fn require(resolver: &Resolver, backend: Backend) -> PathBuf {
    match resolver.resolve(backend) {
        Ok(r) => r.path,
        Err(e) => panic!(
            "backend_missing: {} not found -- {}",
            backend.exe_name(),
            e.remediation
                .and_then(|r| r.manual.or(r.managed))
                .unwrap_or_default()
        ),
    }
}

fn run(input: &Path, output: &Path, tuning: &Tuning) -> exec::Outcome {
    let req = exec::Request {
        from: Format::from_path(input).unwrap(),
        to: Format::from_path(output).unwrap(),
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite: true,
        tuning: tuning.clone(),
        allow_extreme: false,
    };
    exec::run(&req, &Resolver::new(), &mut |_| {})
        .unwrap_or_else(|e| panic!("{} -> {} failed: {e}", input.display(), output.display()))
}

/// The text of a document, as pandoc reads it back.
fn text_of(doc: &Path) -> String {
    let pandoc = require(&Resolver::new(), Backend::Pandoc);
    let out = Command::new(pandoc)
        .arg(doc)
        .args(["-t", "plain", "--wrap=none"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The codec and duration in seconds of a sound file.
fn sound(path: &Path) -> (String, f64) {
    let ffprobe = require(&Resolver::new(), Backend::Ffprobe);
    let probe = convkit_core::probe::run(&ffprobe, path)
        .unwrap_or_else(|e| panic!("ffprobe failed on {}: {e}", path.display()));
    let codec = probe.audio_codec().unwrap_or("none").to_string();
    let secs = probe.duration_ms.unwrap_or(0) as f64 / 1000.0;
    (codec, secs)
}

fn words(n: usize) -> String {
    const SENTENCES: [&str; 6] = [
        "The morning train was late again, so we walked to the river instead.",
        "Seven small boats were tied along the old stone wall.",
        "Nobody knew who had painted the bright red door at the end of the street.",
        "We bought bread, cheese and apples from the market on the square.",
        "In the afternoon the wind picked up and the clouds turned grey.",
        "By evening the station was quiet, and the last train had gone.",
    ];
    let mut out = String::new();
    let mut count = 0;
    let mut i = 0;
    while count < n {
        let s = SENTENCES[i % SENTENCES.len()];
        count += s.split_whitespace().count();
        out.push_str(s);
        out.push(if i % 3 == 2 { '\n' } else { ' ' });
        if i % 3 == 2 {
            out.push('\n');
        }
        i += 1;
    }
    out
}

/// Every engine reads English at roughly 110 to 260 words a minute at its
/// own rate; anything outside that read too little or something else.
fn assert_plausible(secs: f64, words: usize, what: &str) {
    let min = words as f64 / 260.0 * 60.0;
    let max = words as f64 / 110.0 * 60.0;
    assert!(
        (min..=max).contains(&secs),
        "{what}: {secs:.1}s for {words} words, expected {min:.0}-{max:.0}s"
    );
}

#[test]
#[ignore = "requires pandoc and LibreOffice; run with --ignored"]
fn txt_in_any_encoding_comes_out_as_the_same_text() {
    let dir = tempfile::tempdir().unwrap();
    let text = "Grüße aus Köln – “quoted”\n\nZweite Zeile";
    let mut utf16 = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        utf16.extend(u.to_le_bytes());
    }
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend(text.as_bytes());
    // Windows-1252 has every one of these characters.
    let cp1252: Vec<u8> = text
        .chars()
        .map(|c| match c {
            '–' => 0x96,
            '“' => 0x93,
            '”' => 0x94,
            c => c as u32 as u8,
        })
        .collect();
    for (name, bytes) in [("utf16", utf16), ("bom", bom), ("cp1252", cp1252)] {
        let input = dir.path().join(format!("{name}.txt"));
        std::fs::write(&input, bytes).unwrap();
        for ext in ["docx", "md", "html", "rtf"] {
            let output = dir.path().join(format!("{name}.{ext}"));
            let outcome = run(&input, &output, &Tuning::default());
            // pandoc's RTF reader drops the character after a `\u8211-`
            // escape, which its own writer makes; LibreOffice and TextEdit
            // read the file right, so it is read back the way conv reads RTF.
            let back = if ext == "rtf" {
                let txt = dir.path().join(format!("{name}-rtf.txt"));
                run(&output, &txt, &Tuning::default());
                std::fs::read_to_string(&txt).unwrap()
            } else {
                text_of(&output)
            };
            assert!(
                back.contains("Grüße aus Köln – “quoted”"),
                "{name} -> {ext}: {back}"
            );
            assert!(back.contains("Zweite Zeile"), "{name} -> {ext}: {back}");
            let noted = outcome.warnings.iter().any(|w| w.contains("Windows-1252"));
            assert_eq!(
                noted,
                name == "cp1252",
                "{name} -> {ext}: {:?}",
                outcome.warnings
            );
        }
    }
}

#[test]
#[ignore = "requires pandoc and LibreOffice; run with --ignored"]
fn txt_and_html_to_pdf_are_real_pdfs() {
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("note.txt");
    std::fs::write(&txt, words(80)).unwrap();
    let html = dir.path().join("page.html");
    std::fs::write(&html, "<h1>Page</h1><p>Some <b>text</b>.</p>").unwrap();
    for input in [txt, html] {
        let output = input.with_extension("pdf");
        run(&input, &output, &Tuning::default());
        let mut head = [0u8; 5];
        std::fs::File::open(&output)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        assert_eq!(&head, b"%PDF-", "{}", output.display());
    }
}

/// A server that counts every connection made to it, for as long as the
/// test runs.
fn counting_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (format!("http://{addr}"), hits)
}

#[test]
#[ignore = "requires pandoc and LibreOffice; run with --ignored"]
fn a_web_page_is_converted_without_touching_the_network() {
    let (url, hits) = counting_server();
    let dir = tempfile::tempdir().unwrap();
    let page = dir.path().join("page.html");
    std::fs::write(
        &page,
        format!(
            "<html><head><title>Saved page</title></head><body><h1>Heading</h1>\
             <p>A picture <img src=\"{url}/a.png\" alt=\"red square\"> here.</p>\
             <iframe src=\"{url}/frame.html\"></iframe>\
             <p><img src=\"//{}/b.png\"></p></body></html>",
            url.trim_start_matches("http://")
        ),
    )
    .unwrap();
    for ext in ["docx", "rtf", "md", "txt", "pdf"] {
        let output = dir.path().join(format!("page.{ext}"));
        let outcome = run(&page, &output, &Tuning::default());
        if matches!(ext, "docx" | "rtf" | "pdf") {
            assert!(
                outcome
                    .notes
                    .iter()
                    .any(|n| n.starts_with("2 pictures on the web were left out")),
                "{ext}: {:?}",
                outcome.notes
            );
        }
        if ext != "pdf" {
            assert!(text_of(&output).contains("Heading"), "{ext}");
        }
    }
    let md = std::fs::read_to_string(dir.path().join("page.md")).unwrap();
    assert!(md.contains(&format!("![red square]({url}/a.png)")), "{md}");
    assert!(!md.contains("iframe"), "{md}");
    // Give a request that was made time to land before counting.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "conv went online");
}

#[test]
#[ignore = "requires pandoc; run with --ignored"]
fn a_document_takes_in_no_file_outside_its_folder() {
    let (url, hits) = counting_server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), "TOP-SECRET").unwrap();
    let docs = dir.path().join("docs");
    std::fs::create_dir_all(docs.join("sub")).unwrap();
    std::fs::write(docs.join("pic.png"), "PIC").unwrap();
    std::fs::write(docs.join("sub").join("s.png"), "SUB").unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(
        docs.join("notes.md"),
        format!(
            "![](../secret.txt)\n\n![](%2e%2e/secret.txt)\n\n![]({})\n\n![]({url}/x.png)\n\n\
             ![](pic.png)\n\n![](sub/s.png)\n",
            secret.display()
        ),
    )
    .unwrap();
    std::fs::write(
        docs.join("page.html"),
        "<html><body><img src=\"../secret.txt\"><img src=\"pic.png\"></body></html>",
    )
    .unwrap();
    for (input, kept) in [("notes.md", 2), ("page.html", 1)] {
        let output = dir.path().join(format!("{input}.docx"));
        let outcome = run(&docs.join(input), &output, &Tuning::default());
        let zip = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
        let media = zip
            .file_names()
            .filter_map(Result::ok)
            .filter(|n| n.starts_with("word/media/"))
            .count();
        assert_eq!(media, kept, "{input}");
        assert!(
            outcome
                .notes
                .iter()
                .any(|n| n.contains("outside the document's folder")),
            "{input}: {:?}",
            outcome.notes
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "conv went online");
}

#[test]
#[ignore = "requires LibreOffice; run with --ignored"]
fn rtf_comes_out_in_its_own_order() {
    let dir = tempfile::tempdir().unwrap();
    let rtf = dir.path().join("report.rtf");
    // A list just before a table: pandoc 3.10 moves the list to the top.
    std::fs::write(
        &rtf,
        r"{\rtf1\ansi\deff0{\fonttbl{\f0 Helvetica;}}
{\pard\fs36 Title\par}
{\pard Body paragraph.\par}
{\listtext\'95\tab}One\par
{\listtext\'95\tab}Two\par
\trowd\cellx2000\cellx4000\pard\intbl A\cell B\cell\row
\pard End.\par}",
    )
    .unwrap();
    let txt = dir.path().join("report.txt");
    run(&rtf, &txt, &Tuning::default());
    let text = std::fs::read_to_string(&txt).unwrap();
    let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("no {s:?} in {text}"));
    assert!(at("Title") < at("One") && at("One") < at("End."), "{text}");
    assert!(!text.starts_with('\u{feff}'), "no byte-order mark");
    let docx = dir.path().join("report.docx");
    run(&rtf, &docx, &Tuning::default());
    assert!(text_of(&docx).contains("Body paragraph."));
}

#[test]
#[ignore = "requires pandoc; run with --ignored"]
fn documents_become_rtf_and_text() {
    let dir = tempfile::tempdir().unwrap();
    let md = dir.path().join("notes.md");
    std::fs::write(
        &md,
        "# Notes\n\nSome **bold** text and a list:\n\n- one\n- two\n",
    )
    .unwrap();
    let rtf = dir.path().join("notes.rtf");
    run(&md, &rtf, &Tuning::default());
    assert!(std::fs::read_to_string(&rtf)
        .unwrap()
        .starts_with("{\\rtf1"));
    let txt = dir.path().join("notes.txt");
    run(&md, &txt, &Tuning::default());
    let text = std::fs::read_to_string(&txt).unwrap();
    assert!(
        text.contains("Some bold text") && !text.contains("**"),
        "{text}"
    );
}

#[test]
#[ignore = "requires ffmpeg; run with --ignored"]
fn aiff_goes_both_ways() {
    let resolver = Resolver::new();
    let ffmpeg = require(&resolver, Backend::Ffmpeg);
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    let made = Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=2")
        .arg(&wav)
        .output()
        .unwrap();
    assert!(made.status.success());
    let aiff = dir.path().join("tone.aiff");
    run(&wav, &aiff, &Tuning::default());
    let (codec, secs) = sound(&aiff);
    assert_eq!(codec, "pcm_s16be");
    assert!((secs - 2.0).abs() < 0.05, "{secs}");
    let m4a = dir.path().join("tone.m4a");
    run(&aiff, &m4a, &Tuning::default());
    assert_eq!(sound(&m4a).0, "aac");
}

/// The engine this system reads with, resolved, or a clear failure.
fn require_engine() {
    let resolver = Resolver::new();
    require(&resolver, Engine::host().backend());
    require(&resolver, Backend::Ffmpeg);
}

#[test]
#[ignore = "requires the system's speech engine and ffmpeg; run with --ignored"]
fn text_read_aloud_lasts_as_long_as_its_words_in_every_format() {
    require_engine();
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("note.txt");
    let text = words(60);
    std::fs::write(&txt, &text).unwrap();
    let n = text.split_whitespace().count();
    for (ext, codec) in [
        ("m4a", "aac"),
        ("mp3", "mp3"),
        ("wav", "pcm_s16le"),
        ("aiff", "pcm_s16be"),
    ] {
        let out = dir.path().join(format!("note.{ext}"));
        run(&txt, &out, &Tuning::default());
        let (got, secs) = sound(&out);
        assert_eq!(got, codec, "{ext}");
        assert_plausible(secs, n, ext);
    }
}

#[test]
#[ignore = "requires the system's speech engine, pandoc and ffmpeg; run with --ignored"]
fn a_document_is_read_as_its_text_not_its_markup() {
    require_engine();
    let dir = tempfile::tempdir().unwrap();
    let text = words(40);
    let n = text.split_whitespace().count();
    let md = dir.path().join("doc.md");
    std::fs::write(
        &md,
        format!("# Notes\n\n{text}\n\n![a picture](https://example.com/x.png)\n"),
    )
    .unwrap();
    let html = dir.path().join("doc.html");
    std::fs::write(
        &html,
        format!("<h1>Notes</h1><p>{text}</p><script>var a = 1;</script>"),
    )
    .unwrap();
    for input in [md, html] {
        let out = input.with_extension("wav");
        run(&input, &out, &Tuning::default());
        // The heading and the picture's words come on top.
        assert_plausible(sound(&out).1, n + 3, &input.display().to_string());
    }
}

#[test]
#[ignore = "requires the system's speech engine and ffmpeg; run with --ignored"]
fn brackets_in_the_text_are_read_not_obeyed() {
    require_engine();
    if Engine::host() == Engine::Sapi {
        return; // System.Speech reads plain text as it is.
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.txt");
    let wiki = dir.path().join("wiki.txt");
    std::fs::write(
        &plain,
        "Read the Main Page and the Help Page before you start.",
    )
    .unwrap();
    std::fs::write(
        &wiki,
        "Read the [[Main Page]] and the [[Help Page]] before you start.",
    )
    .unwrap();
    let a = dir.path().join("plain.wav");
    let b = dir.path().join("wiki.wav");
    run(&plain, &a, &Tuning::default());
    run(&wiki, &b, &Tuning::default());
    let (a, b) = (sound(&a).1, sound(&b).1);
    assert!(
        b > a * 0.8,
        "the bracketed words went unread: {b:.2}s against {a:.2}s"
    );
}

#[test]
#[ignore = "requires the system's speech engine and ffmpeg; run with --ignored"]
fn a_long_text_is_read_to_the_end() {
    require_engine();
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("long.txt");
    let text = words(900);
    std::fs::write(&txt, &text).unwrap();
    let out = dir.path().join("long.m4a");
    run(&txt, &out, &Tuning::default());
    assert_plausible(sound(&out).1, text.split_whitespace().count(), "900 words");
}

#[test]
#[ignore = "requires the system's speech engine and ffmpeg; run with --ignored"]
fn the_rate_changes_how_long_it_takes() {
    require_engine();
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("rate.txt");
    std::fs::write(&txt, words(40)).unwrap();
    let slow = dir.path().join("slow.wav");
    let fast = dir.path().join("fast.wav");
    run(&txt, &slow, &speech_tuning(None, Some(120)));
    run(&txt, &fast, &speech_tuning(None, Some(300)));
    let (slow, fast) = (sound(&slow).1, sound(&fast).1);
    assert!(
        fast < slow * 0.7,
        "300 wpm took {fast:.1}s, 120 wpm {slow:.1}s"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS say and ffmpeg; run with --ignored"]
fn say_reads_with_a_named_voice_and_lists_its_voices() {
    let resolver = Resolver::new();
    require(&resolver, Backend::Say);
    let voices = convkit_core::speech::voices(&resolver, Engine::Say).unwrap();
    assert!(voices.len() > 5, "{voices:?}");
    let english = voices
        .iter()
        .find(|v| v.language.starts_with("en_"))
        .expect("an English voice");
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("hello.txt");
    std::fs::write(&txt, "Hello there, this is a named voice.").unwrap();
    let out = dir.path().join("hello.aiff");
    let outcome = run(&txt, &out, &speech_tuning(Some(&english.name), None));
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert_eq!(sound(&out).0, "pcm_s16be");
    let e = exec::run(
        &exec::Request {
            from: Format::Txt,
            to: Format::Aiff,
            inputs: vec![txt.clone()],
            output: dir.path().join("x.aiff"),
            overwrite: true,
            tuning: speech_tuning(Some("No Such Voice Anywhere"), None),
            allow_extreme: false,
        },
        &resolver,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInvocation);
}
