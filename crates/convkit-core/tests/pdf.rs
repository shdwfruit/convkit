//! `conv merge`, `conv split`, `conv pages` and PDF passwords against a
//! real qpdf.
//!
//! `#[ignore]`-gated like `output_properties.rs`: `cargo test` stays green
//! without qpdf, and `cargo test -- --ignored` runs these wherever qpdf
//! resolves (PATH, `conv install qpdf`, or `CONVKIT_QPDF`).

use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::pdf::{
    self, parse_list, parse_range, Lock, PageEdit, Password, PdfOutcome, Restriction, Rewrite,
};
use convkit_core::rotate::Turn;
use convkit_core::{Backend, ErrorCode, ResolvedBackend, Resolver};

fn qpdf() -> ResolvedBackend {
    Resolver::new().resolve(Backend::Qpdf).unwrap_or_else(|e| {
        let hint = e
            .remediation
            .as_ref()
            .and_then(|r| r.managed.as_deref().or(r.manual.as_deref()))
            .unwrap_or("no remediation available");
        panic!("backend_missing: qpdf not found -- {hint}")
    })
}

fn sample() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample.pdf");
    assert!(
        p.is_file(),
        "missing tests/fixtures/sample.pdf; see tests/fixtures/sample.typ"
    );
    p
}

fn copy_sample(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::copy(sample(), &p).unwrap();
    p
}

/// Runs qpdf directly (to prepare inputs or inspect outputs); 0 and 3 are
/// both success.
fn qpdf_ok(args: &[&str]) -> String {
    let out = Command::new(&qpdf().path).args(args).output().unwrap();
    assert!(
        matches!(out.status.code(), Some(0) | Some(3)),
        "qpdf {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn page_count(p: &Path) -> usize {
    qpdf_ok(&["--show-npages", s(p)]).trim().parse().unwrap()
}

/// Each page's width in points, in page order (the fixture's pages are 210,
/// 220 and 230 wide).
fn widths(p: &Path) -> Vec<i64> {
    let v: serde_json::Value = serde_json::from_str(&qpdf_ok(&[
        "--json",
        "--json-key=pages",
        "--json-key=qpdf",
        s(p),
    ]))
    .unwrap();
    let objects = &v["qpdf"][1];
    v["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| {
            let key = format!("obj:{}", page["object"].as_str().unwrap());
            objects[&key]["value"]["/MediaBox"][2]
                .as_f64()
                .unwrap()
                .round() as i64
        })
        .collect()
}

/// Each page's `/Rotate`, in page order.
fn rotations(p: &Path) -> Vec<i64> {
    let v: serde_json::Value = serde_json::from_str(&qpdf_ok(&[
        "--json",
        "--json-key=pages",
        "--json-key=qpdf",
        s(p),
    ]))
    .unwrap();
    let objects = &v["qpdf"][1];
    v["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| {
            let key = format!("obj:{}", page["object"].as_str().unwrap());
            objects[&key]["value"]["/Rotate"].as_i64().unwrap_or(0)
        })
        .collect()
}

/// qpdf's exit status for `args`.
fn qpdf_code(args: &[&str]) -> i32 {
    Command::new(&qpdf().path)
        .args(args)
        .output()
        .unwrap()
        .status
        .code()
        .unwrap()
}

/// `qpdf --requires-password`: 0 needs one, 2 is not encrypted, 3 opens
/// without one but is encrypted.
fn requires_password(p: &Path) -> i32 {
    qpdf_code(&["--requires-password", s(p)])
}

/// The fixture locked with `open` (and `owner` as its owner password).
fn locked_sample(dir: &Path, name: &str) -> PathBuf {
    let out = dir.join(name);
    qpdf_ok(&[
        "--encrypt",
        "--user-password=open",
        "--owner-password=owner",
        "--bits=256",
        "--",
        s(&sample()),
        s(&out),
    ]);
    out
}

fn pw(s: &str) -> Password {
    Password::new(s).unwrap()
}

/// An asker that never has a password.
fn none() -> impl FnMut(&Path, bool) -> Option<Password> {
    |_: &Path, _| None
}

/// An asker that gives `password` once per file.
fn gives(password: &'static str) -> impl FnMut(&Path, bool) -> Option<Password> {
    move |_: &Path, wrong| (!wrong).then(|| pw(password))
}

fn bookmarks(p: &Path) -> usize {
    let v: serde_json::Value =
        serde_json::from_str(&qpdf_ok(&["--json", "--json-key=outlines", s(p)])).unwrap();
    v["outlines"].as_array().unwrap().len()
}

fn encrypted(p: &Path) -> bool {
    !qpdf_ok(&["--show-encryption", s(p)]).starts_with("File is not encrypted")
}

/// `n` copies of the fixture merged into one file, by qpdf directly.
fn many_pages(dir: &Path, n: usize) -> PathBuf {
    let out = dir.join("doc.pdf");
    let sample = sample();
    // The fixture is the primary input (not `--empty`) so its bookmarks
    // carry over, as they would in a real document.
    let mut args = vec![s(&sample), "--pages", "."];
    args.extend(std::iter::repeat_n(s(&sample), n - 1));
    args.push("--");
    args.push(s(&out));
    qpdf_ok(&args);
    out
}

fn merge(inputs: &[PathBuf], output: &Path, overwrite: bool) -> PdfOutcome {
    let q = qpdf();
    let plan = pdf::plan_merge(&q.path, inputs, output, &mut none()).unwrap();
    pdf::run(&plan, &q, overwrite, &mut |_| {}).unwrap()
}

fn split(input: &Path, ranges: &[&str], outdir: Option<&Path>) -> PdfOutcome {
    let q = qpdf();
    let ranges: Vec<_> = ranges.iter().map(|r| parse_range(r).unwrap()).collect();
    let plan = pdf::plan_split(&q.path, input, &ranges, outdir, &mut none()).unwrap();
    pdf::run(&plan, &q, false, &mut |_| {}).unwrap()
}

/// `conv pages` on `input`: `keep` and `delete` as typed, and turns as
/// `--rotate` takes them.
fn pages(input: &Path, keep: &[&str], delete: &str, turns: &[&str]) -> PdfOutcome {
    let q = qpdf();
    let edit = PageEdit {
        keep: keep.iter().flat_map(|k| parse_list(k).unwrap()).collect(),
        delete: if delete.is_empty() {
            vec![]
        } else {
            parse_list(delete).unwrap()
        },
        turns: turns
            .iter()
            .map(|t| pdf::pages::parse_turn(t).unwrap())
            .collect(),
    };
    let plan = pdf::plan_pages(&q.path, input, &edit, None, None, &mut none()).unwrap();
    pdf::run(&plan, &q, false, &mut |_| {}).unwrap()
}

/// Writes `input` again as `output`, locked with `lock` or unlocked.
fn rewrite(
    input: &Path,
    output: &Path,
    lock: Option<Lock>,
    ask: &mut dyn FnMut(&Path, bool) -> Option<Password>,
) -> PdfOutcome {
    let q = qpdf();
    let how = if lock.is_some() {
        Rewrite::Lock
    } else {
        Rewrite::Unlock
    };
    let mut plan = pdf::plan_rewrite(&q.path, input, output, how, ask).unwrap();
    if let Some(lock) = lock {
        plan.set_lock(lock);
    }
    pdf::run(&plan, &q, false, &mut |_| {}).unwrap()
}

fn names(o: &PdfOutcome) -> Vec<String> {
    o.outputs
        .iter()
        .map(|w| w.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn merge_joins_every_page_in_order_and_keeps_the_first_files_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let a = copy_sample(dir.path(), "a.pdf");
    let b = copy_sample(dir.path(), "b.pdf");
    let out = dir.path().join("out.pdf");

    let o = merge(&[a, b], &out, false);

    assert_eq!(page_count(&out), 6);
    assert_eq!(widths(&out), vec![210, 220, 230, 210, 220, 230]);
    assert_eq!(
        bookmarks(&out),
        bookmarks(&sample()),
        "a.pdf's bookmarks survive"
    );
    assert_eq!(
        o.notes,
        vec!["Bookmarks from b.pdf are not carried over; out.pdf keeps a.pdf's."]
    );
    assert_eq!(o.outputs[0].pages.len(), 6);
    assert!(o.outputs[0].bytes > 0);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn merge_drops_permission_restrictions_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let restricted = dir.path().join("locked.pdf");
    let sample = sample();
    qpdf_ok(&[
        "--encrypt",
        "--user-password=",
        "--owner-password=owner",
        "--bits=256",
        "--print=none",
        "--",
        s(&sample),
        s(&restricted),
    ]);
    let out = dir.path().join("out.pdf");

    let o = merge(&[restricted, sample], &out, false);

    assert!(!encrypted(&out));
    assert!(o.notes.contains(
        &"locked.pdf restricts printing, copying or editing; out.pdf does not.".to_string()
    ));
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn merge_can_replace_one_of_its_own_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let cover = copy_sample(dir.path(), "cover.pdf");
    let report = copy_sample(dir.path(), "report.pdf");

    merge(&[cover, report.clone()], &report, true);

    assert_eq!(page_count(&report), 6);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn merge_passes_awkward_file_names_through_to_qpdf() {
    let dir = tempfile::tempdir().unwrap();
    let first = copy_sample(dir.path(), "-cover.pdf");
    let second = copy_sample(dir.path(), "my report \u{e9}.pdf");
    let third = copy_sample(dir.path(), "@notes.pdf");
    let out = dir.path().join("out.pdf");

    // Relative names, run from inside the folder, are the case that needs
    // `./` in front of `-` and `@`.
    let q = qpdf();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let rel = |p: &Path| PathBuf::from(p.file_name().unwrap());
    let result = pdf::plan_merge(
        &q.path,
        &[rel(&first), rel(&second), rel(&third)],
        Path::new("out.pdf"),
        &mut none(),
    )
    .and_then(|plan| pdf::run(&plan, &q, false, &mut |_| {}));
    std::env::set_current_dir(cwd).unwrap();

    result.unwrap();
    assert_eq!(page_count(&out), 9);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_password_protected_pdf_needs_its_password() {
    let dir = tempfile::tempdir().unwrap();
    let locked = locked_sample(dir.path(), "secret.pdf");
    let out = dir.path().join("out.pdf");
    let q = qpdf();

    let e = pdf::plan_merge(&q.path, &[locked.clone(), sample()], &out, &mut none()).unwrap_err();
    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert_eq!(e.message, "secret.pdf is password-protected");

    let mut tries = vec![pw("open"), pw("guess")];
    let mut asked = Vec::new();
    let plan = pdf::plan_merge(&q.path, &[locked, sample()], &out, &mut |p, wrong| {
        asked.push((p.file_name().unwrap().to_owned(), wrong));
        tries.pop()
    })
    .unwrap();
    assert_eq!(
        asked,
        vec![("secret.pdf".into(), false), ("secret.pdf".into(), true)]
    );
    let o = pdf::run(&plan, &q, false, &mut |_| {}).unwrap();

    assert_eq!(page_count(&out), 6);
    assert!(!encrypted(&out));
    assert!(o.notes.contains(
        &"secret.pdf was password-protected; out.pdf isn't. Add --password to lock it.".to_string()
    ));
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_wrong_password_is_refused_with_nothing_written() {
    let dir = tempfile::tempdir().unwrap();
    let locked = locked_sample(dir.path(), "secret.pdf");
    let mut once = Some(pw("nope"));
    let e = pdf::plan_split(&qpdf().path, &locked, &[], None, &mut |_, _| once.take()).unwrap_err();
    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert_eq!(e.message, "wrong password for secret.pdf");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_lock_needs_its_password_to_open_and_is_aes_256() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let out = dir.path().join("report-locked.pdf");

    let o = rewrite(
        &input,
        &out,
        Some(Lock::new(pw("pa ss"), &[]).unwrap()),
        &mut none(),
    );

    assert!(o.locked);
    assert_eq!(requires_password(&out), 0, "it needs a password to open");
    assert_eq!(qpdf_code(&["--show-npages", s(&out)]), 2);
    assert_eq!(
        qpdf_code(&["--password=wrong", "--show-npages", s(&out)]),
        2
    );
    assert_eq!(
        qpdf_ok(&["--password=pa ss", "--show-npages", s(&out)]).trim(),
        "3"
    );
    let shown = qpdf_ok(&["--password=pa ss", "--show-encryption", s(&out)]);
    assert!(shown.contains("R = 6"), "{shown}");
    assert!(shown.contains("stream encryption method: AESv3"), "{shown}");
    assert!(
        shown.contains("Supplied password is owner password"),
        "{shown}"
    );
    assert!(o.notes.is_empty(), "{:?}", o.notes);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_restricted_lock_turns_off_what_it_says_and_the_password_cannot_lift_it() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let out = dir.path().join("report-locked.pdf");

    let lock = Lock::new(pw("pw"), &[Restriction::Print, Restriction::Copy]).unwrap();
    let o = rewrite(&input, &out, Some(lock), &mut none());

    let shown = qpdf_ok(&["--password=pw", "--show-encryption", s(&out)]);
    assert!(
        shown.contains("Supplied password is user password"),
        "{shown}"
    );
    assert!(
        shown.contains("print high resolution: not allowed"),
        "{shown}"
    );
    assert!(
        shown.contains("extract for any purpose: not allowed"),
        "{shown}"
    );
    assert!(shown.contains("modify anything: allowed"), "{shown}");
    assert_eq!(
        o.notes,
        vec![
            "report-locked.pdf opens with the password but turns off printing and copying; \
              PDF apps honour that, though nothing forces them to."
        ]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn an_unlock_writes_a_copy_that_opens_without_a_password() {
    let dir = tempfile::tempdir().unwrap();
    let locked = locked_sample(dir.path(), "secret.pdf");
    let out = dir.path().join("secret-unlocked.pdf");

    let o = rewrite(&locked, &out, None, &mut gives("open"));

    assert_eq!(requires_password(&out), 2, "not encrypted at all");
    assert_eq!(widths(&out), vec![210, 220, 230]);
    assert_eq!(bookmarks(&out), bookmarks(&sample()));
    assert!(!o.locked);
    assert!(o.notes.is_empty(), "{:?}", o.notes);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn an_owner_password_only_pdf_unlocks_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let restricted = dir.path().join("form.pdf");
    qpdf_ok(&[
        "--encrypt",
        "--user-password=",
        "--owner-password=owner",
        "--bits=256",
        "--print=none",
        "--",
        s(&sample()),
        s(&restricted),
    ]);
    assert_eq!(requires_password(&restricted), 3);
    let out = dir.path().join("form-unlocked.pdf");

    let o = rewrite(&restricted, &out, None, &mut |_, _| {
        panic!("asked for a password")
    });

    assert_eq!(requires_password(&out), 2);
    assert_eq!(
        o.notes,
        vec!["form.pdf restricts printing, copying or editing; form-unlocked.pdf does not."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn unlocking_a_pdf_without_a_password_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "plain.pdf");
    let e = pdf::plan_rewrite(
        &qpdf().path,
        &input,
        &dir.path().join("plain-unlocked.pdf"),
        Rewrite::Unlock,
        &mut none(),
    )
    .unwrap_err();
    assert_eq!(
        e.message,
        "plain.pdf has no password or restrictions to remove"
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_and_merge_lock_every_file_they_write() {
    let dir = tempfile::tempdir().unwrap();
    let input = locked_sample(dir.path(), "secret.pdf");
    let q = qpdf();

    let mut plan = pdf::plan_split(&q.path, &input, &[], None, &mut gives("open")).unwrap();
    plan.set_lock(Lock::new(pw("new"), &[]).unwrap());
    let o = pdf::run(&plan, &q, false, &mut |_| {}).unwrap();
    assert_eq!(
        names(&o),
        vec!["secret-1.pdf", "secret-2.pdf", "secret-3.pdf"]
    );
    for w in &o.outputs {
        assert_eq!(requires_password(&w.path), 0, "{}", w.path.display());
        assert_eq!(
            qpdf_code(&["--password=open", "--show-npages", s(&w.path)]),
            2
        );
        assert_eq!(
            qpdf_ok(&["--password=new", "--show-npages", s(&w.path)]).trim(),
            "1"
        );
    }
    assert!(
        !o.notes.iter().any(|n| n.contains("password-protected")),
        "{:?}",
        o.notes
    );

    let out = dir.path().join("all.pdf");
    let mut plan = pdf::plan_merge(&q.path, &[sample(), input], &out, &mut gives("open")).unwrap();
    plan.set_lock(Lock::new(pw("new"), &[]).unwrap());
    pdf::run(&plan, &q, false, &mut |_| {}).unwrap();
    assert_eq!(requires_password(&out), 0);
    assert_eq!(
        qpdf_ok(&["--password=new", "--show-npages", s(&out)]).trim(),
        "6"
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn pages_writes_the_listed_pages_in_order_with_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let doc = many_pages(dir.path(), 2); // 6 pages: 210 220 230 210 220 230

    let o = pages(&doc, &["3", "1-2", "5-z"], "", &[]);

    assert_eq!(names(&o), vec!["doc-3_1-2_5-6.pdf"]);
    let out = &o.outputs[0].path;
    assert_eq!(page_count(out), 5);
    assert_eq!(widths(out), vec![230, 210, 220, 220, 230]);
    assert_eq!(o.outputs[0].pages, vec![3, 1, 2, 5, 6]);

    let o = pages(&doc, &["2,2", "6-4"], "", &[]);
    assert_eq!(widths(&o.outputs[0].path), vec![220, 220, 230, 220, 210]);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn pages_deletes_and_keeps_the_documents_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");

    let o = pages(&input, &[], "2", &[]);

    let out = &o.outputs[0].path;
    assert_eq!(names(&o), vec!["sample-1_3.pdf"]);
    assert_eq!(widths(out), vec![210, 230]);
    assert_eq!(bookmarks(out), bookmarks(&sample()), "the outline stays");
    assert_eq!(
        o.notes,
        vec!["1 bookmark led to a page that was left out; it is still listed but goes nowhere."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn pages_turns_the_pages_asked_for_by_their_number_in_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");

    let o = pages(&input, &["3", "1-2"], "", &["right:1", "180:3", "left:2"]);

    let out = &o.outputs[0].path;
    assert_eq!(names(&o), vec!["sample-3_1-2-rotated.pdf"]);
    assert_eq!(widths(out), vec![230, 210, 220]);
    assert_eq!(rotations(out), vec![180, 90, 270]);

    // A turn adds to the page's own: turning it back undoes it.
    let back = pages(out, &[], "", &["left:2"]);
    assert_eq!(rotations(&back.outputs[0].path), vec![180, 0, 270]);

    let all = pages(&input, &[], "", &["left"]);
    assert_eq!(names(&all), vec!["sample-rotated.pdf"]);
    assert_eq!(rotations(&all.outputs[0].path), vec![270, 270, 270]);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn pages_opens_a_locked_pdf_and_can_lock_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let input = locked_sample(dir.path(), "secret.pdf");
    let q = qpdf();
    let edit = PageEdit {
        keep: parse_list("2").unwrap(),
        turns: vec![(Turn::Right, None)],
        ..PageEdit::default()
    };

    let plan = pdf::plan_pages(&q.path, &input, &edit, None, None, &mut gives("open")).unwrap();
    let o = pdf::run(&plan, &q, false, &mut |_| {}).unwrap();
    let out = &o.outputs[0].path;
    assert_eq!(names(&o), vec!["secret-2-rotated.pdf"]);
    assert_eq!(requires_password(out), 2);
    assert_eq!((widths(out), rotations(out)), (vec![220], vec![90]));

    let mut plan = pdf::plan_pages(
        &q.path,
        &input,
        &edit,
        Some(&dir.path().join("two.pdf")),
        None,
        &mut gives("open"),
    )
    .unwrap();
    plan.set_lock(Lock::new(pw("new"), &[]).unwrap());
    pdf::run(&plan, &q, false, &mut |_| {}).unwrap();
    assert_eq!(requires_password(&dir.path().join("two.pdf")), 0);
}

/// What macOS's own PDF engine makes of a lock: it is locked, only its
/// password opens it, and the restrictions it asked for are kept.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires qpdf and swift; run with --ignored"]
fn pdfkit_needs_the_password_and_sees_the_restrictions() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let out = dir.path().join("report-locked.pdf");
    let lock = Lock::new(pw("open sesame"), &[Restriction::Print]).unwrap();
    rewrite(&input, &out, Some(lock), &mut none());

    let script = dir.path().join("check.swift");
    std::fs::write(
        &script,
        "import PDFKit\n\
         let a = CommandLine.arguments\n\
         let doc = PDFDocument(url: URL(fileURLWithPath: a[1]))!\n\
         print(\"locked=\\(doc.isLocked)\")\n\
         print(\"wrong=\\(doc.unlock(withPassword: \"nope\"))\")\n\
         print(\"right=\\(doc.unlock(withPassword: a[2]))\")\n\
         print(\"pages=\\(doc.pageCount) print=\\(doc.allowsPrinting) copy=\\(doc.allowsCopying)\")\n",
    )
    .unwrap();
    let run = Command::new("swift")
        .arg(&script)
        .arg(&out)
        .arg("open sesame")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        said.lines().collect::<Vec<_>>(),
        vec![
            "locked=true",
            "wrong=false",
            "right=true",
            "pages=3 print=false copy=true"
        ]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_damaged_pdf_is_repaired_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.pdf");
    let sample = sample();
    qpdf_ok(&["--object-streams=disable", s(&sample), s(&plain)]);
    let mut bytes = std::fs::read(&plain).unwrap();
    bytes.truncate(bytes.len() - 40); // cut off the trailer and startxref
    let damaged = dir.path().join("damaged.pdf");
    std::fs::write(&damaged, bytes).unwrap();

    let o = merge(&[damaged, sample], &dir.path().join("out.pdf"), false);

    assert_eq!(
        o.warnings,
        vec!["damaged.pdf was damaged and has been repaired; check out.pdf."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_writes_one_file_per_page_padded_to_the_page_count() {
    let dir = tempfile::tempdir().unwrap();
    let doc = many_pages(dir.path(), 4);
    let out = dir.path().join("pages");
    std::fs::create_dir(&out).unwrap();

    let o = split(&doc, &[], Some(&out));

    let n = names(&o);
    assert_eq!(n.len(), 12);
    assert_eq!(
        (n[0].as_str(), n[11].as_str()),
        ("doc-01.pdf", "doc-12.pdf")
    );
    assert_eq!(widths(&out.join("doc-02.pdf")), vec![220]);
    assert_eq!(
        o.notes,
        vec!["doc.pdf's bookmarks are not carried into the split files."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_into_a_folder_with_a_percent_in_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");
    let out = dir.path().join("50%done");
    std::fs::create_dir(&out).unwrap();

    let o = split(&input, &[], Some(&out));

    assert_eq!(
        names(&o),
        vec!["sample-1.pdf", "sample-2.pdf", "sample-3.pdf"]
    );
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 3);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_pads_to_three_digits_past_ninety_nine_pages() {
    let dir = tempfile::tempdir().unwrap();
    let doc = many_pages(dir.path(), 34); // 102 pages
    let out = dir.path().join("pages");
    std::fs::create_dir(&out).unwrap();

    let o = split(&doc, &[], Some(&out));

    let n = names(&o);
    assert_eq!(n.len(), 102);
    assert_eq!(
        (n[0].as_str(), n[101].as_str()),
        ("doc-001.pdf", "doc-102.pdf")
    );
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 102);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_ranges_keep_page_order_including_reversed_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");

    let o = split(&input, &["3-1", "2-z"], None);

    assert_eq!(names(&o), vec!["sample-3-1.pdf", "sample-2-3.pdf"]);
    assert_eq!(
        widths(&dir.path().join("sample-3-1.pdf")),
        vec![230, 220, 210]
    );
    assert_eq!(widths(&dir.path().join("sample-2-3.pdf")), vec![220, 230]);
    assert!(o.warnings.is_empty());
    assert!(o.notes.contains(
        &"Pages 2-3 are in more than one range, so they appear in more than one file.".to_string()
    ));
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_leaves_out_pages_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");

    let o = split(&input, &["2"], None);

    assert_eq!(names(&o), vec!["sample-2.pdf"]);
    assert_eq!(
        o.warnings,
        vec!["Pages 1 and 3 are not in any range, so they were left out."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn split_refuses_when_any_output_exists_and_writes_none() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "sample.pdf");
    std::fs::write(dir.path().join("sample-2.pdf"), b"already here").unwrap();
    let q = qpdf();
    let ranges = [parse_range("1").unwrap(), parse_range("2").unwrap()];
    let plan = pdf::plan_split(&q.path, &input, &ranges, None, &mut none()).unwrap();

    let e = pdf::run(&plan, &q, false, &mut |_| {}).unwrap_err();

    assert_eq!(e.code, ErrorCode::OutputExists);
    assert!(!dir.path().join("sample-1.pdf").exists());
}

// --- A PDF's pages as images ------------------------------------------------
//
// Real qpdf and Typst, plus ffmpeg for JPEG and ImageMagick for WebP. The
// fixture's pages are 210, 220 and 230 points wide and 297 tall, so each
// page's file can be told apart by its size, and every render is small.

use convkit_core::pdf::raster::PagesReport;
use convkit_core::{exec, registry, Format, Tuning};

fn require(backends: &[Backend]) -> Resolver {
    let resolver = Resolver::new();
    for &b in backends {
        if let Err(e) = resolver.resolve(b) {
            let hint = e
                .remediation
                .as_ref()
                .and_then(|r| r.managed.as_deref().or(r.manual.as_deref()))
                .unwrap_or("no remediation available");
            panic!("backend_missing: {} not found -- {hint}", b.exe_name());
        }
    }
    resolver
}

fn drawn_pages(
    input: &Path,
    output: &Path,
    tuning: Tuning,
    overwrite: bool,
) -> convkit_core::Result<exec::Outcome> {
    let to = Format::from_path(output).unwrap();
    let resolver = require(&registry::backends_for(Format::Pdf, to));
    let req = exec::Request {
        from: Format::Pdf,
        to,
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite,
        tuning,
        allow_extreme: false,
    };
    exec::run(&req, &resolver, &mut |_| {})
}

fn report(o: &exec::Outcome) -> &PagesReport {
    o.pages.as_ref().expect("a PDF's pages report on them")
}

/// A PNG's size and pixels as 8-bit RGBA.
fn decode_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    let rgba = match reader.output_color_type().0 {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        other => panic!("unexpected PNG colour type {other:?}"),
    };
    (info.width, info.height, rgba)
}

/// The share of pixels in `(x0, y0)..(x1, y1)` that are dark.
fn dark_share((w, _, rgba): &(u32, u32, Vec<u8>), (x0, y0, x1, y1): (u32, u32, u32, u32)) -> f64 {
    let mut dark = 0usize;
    let mut all = 0usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 4) as usize;
            let luma =
                (u32::from(rgba[i]) * 3 + u32::from(rgba[i + 1]) * 6 + u32::from(rgba[i + 2])) / 10;
            all += 1;
            dark += usize::from(luma < 128);
        }
    }
    dark as f64 / all as f64
}

/// A JPEG's size, from its frame header.
fn jpeg_size(path: &Path) -> (u32, u32) {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[..2], [0xFF, 0xD8], "{} is a JPEG", path.display());
    let mut i = 2;
    while i + 9 < b.len() {
        assert_eq!(b[i], 0xFF);
        let marker = b[i + 1];
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        if matches!(marker, 0xC0..=0xC2) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
            return (u32::from(w), u32::from(h));
        }
        i += 2 + len;
    }
    panic!("no frame header in {}", path.display());
}

/// Each page's width at `dpi`, as conv asks Typst for it, and the height a
/// 297-point page then has: Typst rounds it from the width, so it may be a
/// pixel off the page's own height at that resolution.
fn at_dpi(width_pt: u32, dpi: u32) -> (u32, u32) {
    let w = (f64::from(width_pt) * f64::from(dpi) / 72.0).round();
    (w as u32, (w * 297.0 / f64::from(width_pt)).round() as u32)
}

fn assert_near((w, h): (u32, u32), (ew, eh): (u32, u32), what: &str) {
    assert_eq!(w, ew, "{what} width");
    assert!(
        h.abs_diff(eh) <= 1,
        "{what} is {w}x{h}, expected about {ew}x{eh}"
    );
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn every_page_becomes_a_png_at_the_dpi_in_a_folder_named_after_the_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");

    let o = drawn_pages(
        &input,
        &dir.path().join("report.png"),
        Tuning::default(),
        false,
    )
    .unwrap();

    let folder = dir.path().join("report");
    assert_eq!(o.output, folder);
    let r = report(&o);
    assert!(r.folder);
    assert_eq!(r.count, Some(3));
    assert_eq!(r.files, ["report-1.png", "report-2.png", "report-3.png"]);
    assert_eq!(r.dpi, Some(150));
    let mut total = 0;
    for (name, width) in r.files.iter().zip([210, 220, 230]) {
        let path = folder.join(name);
        let png = decode_png(&path);
        assert_near((png.0, png.1), at_dpi(width, 150), name);
        total += std::fs::metadata(&path).unwrap().len();
    }
    assert_eq!(o.bytes, total);
    // The heading and the line of text are drawn: not a blank page.
    let first = decode_png(&folder.join("report-1.png"));
    let share = dark_share(&first, (0, 0, first.0, first.1 / 3));
    assert!(share > 0.002, "page 1 looks blank: {share}");
    assert!(share < 0.5, "page 1 looks filled in: {share}");
    // Nothing else is left beside the folder.
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["report", "report.pdf"]);
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn the_dpi_sets_the_size_and_one_page_picked_is_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let tuning = Tuning {
        dpi: Some(300),
        pages: Some(pdf::parse_list("2").unwrap()),
        ..Tuning::default()
    };

    let out = dir.path().join("report-2.png");
    let o = drawn_pages(&input, &out, tuning, false).unwrap();

    assert_eq!(o.output, out);
    assert!(!report(&o).folder);
    let png = decode_png(&out);
    assert_near((png.0, png.1), at_dpi(220, 300), "page 2 at 300 dpi");
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn a_resize_fits_each_page_within_the_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let tuning = Tuning {
        resize: Some("x300".to_string()),
        ..Tuning::default()
    };

    let o = drawn_pages(&input, &dir.path().join("report.png"), tuning, false).unwrap();

    for name in &report(&o).files {
        let (w, h, _) = decode_png(&o.output.join(name));
        assert!((299..=300).contains(&h), "{name} is {w}x{h}");
    }
    assert_eq!(report(&o).dpi, None);
}

#[test]
#[ignore = "requires qpdf, typst and ffmpeg; run with --ignored"]
fn jpeg_pages_of_different_sizes_are_each_their_own_size() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");

    let o = drawn_pages(
        &input,
        &dir.path().join("report.jpg"),
        Tuning::default(),
        false,
    )
    .unwrap();

    let r = report(&o);
    assert_eq!(r.files, ["report-1.jpg", "report-2.jpg", "report-3.jpg"]);
    for (name, width) in r.files.iter().zip([210, 220, 230]) {
        assert_near(jpeg_size(&o.output.join(name)), at_dpi(width, 150), name);
    }
}

#[test]
#[ignore = "requires qpdf, typst and ImageMagick; run with --ignored"]
fn webp_pages_are_written_by_imagemagick() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let tuning = Tuning {
        pages: Some(pdf::parse_list("1,3").unwrap()),
        quality: Some(80),
        ..Tuning::default()
    };

    let o = drawn_pages(&input, &dir.path().join("report.webp"), tuning, false).unwrap();

    let r = report(&o);
    assert_eq!(r.pages, [1, 3]);
    for name in &r.files {
        let b = std::fs::read(o.output.join(name)).unwrap();
        assert_eq!((&b[..4], &b[8..12]), (&b"RIFF"[..], &b"WEBP"[..]), "{name}");
    }
}

/// One page, 200 points square: a red square annotation drawn by its own
/// appearance stream, and a text field holding HELLO with no appearance at
/// all (`NeedAppearances`), as some form fillers leave one. Typst draws
/// neither by itself.
fn annotated_pdf(path: &Path) {
    let square = "1 0 0 rg 0 0 60 60 re f";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [6 0 R] /NeedAppearances true \
         /DA (/Helv 0 Tf 0 g) /DR << /Font << /Helv 7 0 R >> >> >> >>"
            .to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
         /Annots [5 0 R 6 0 R] /Resources << >> >>"
            .to_string(),
        "<< /Length 0 >>\nstream\n\nendstream".to_string(),
        "<< /Type /Annot /Subtype /Square /Rect [20 20 80 80] /C [1 0 0] /IC [1 0 0] /F 4 \
         /AP << /N 8 0 R >> >>"
            .to_string(),
        "<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V (HELLO) /F 4 \
         /Rect [100 120 190 150] /DA (/Helv 18 Tf 0 g) /P 3 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
        format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 60 60] /Length {} >>\nstream\n{square}\n\
             endstream",
            square.len() + 1
        ),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).bytes());
    }
    let xref = pdf.len();
    pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for o in offsets {
        pdf.extend(format!("{o:010} 00000 n \n").bytes());
    }
    pdf.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .bytes(),
    );
    std::fs::write(path, pdf).unwrap();
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn annotations_and_form_fields_are_drawn_on_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("form.pdf");
    annotated_pdf(&input);

    let o = drawn_pages(
        &input,
        &dir.path().join("form.png"),
        Tuning::default(),
        false,
    )
    .unwrap();

    assert_eq!(o.output, dir.path().join("form.png"), "one page, one file");
    let png = decode_png(&o.output);
    assert_eq!((png.0, png.1), (417, 417));
    let px = |x: u32, y: u32| {
        let i = ((y * png.0 + x) * 4) as usize;
        (png.2[i], png.2[i + 1], png.2[i + 2])
    };
    // The square spans 20-80 points from the bottom left: 42-167 px across
    // and 250-375 down at 150 dpi.
    let (r, g, b) = px(104, 312);
    assert!(
        r > 200 && g < 60 && b < 60,
        "the square annotation is red: {r},{g},{b}"
    );
    assert_eq!(px(10, 10), (255, 255, 255), "the page is white");
    // HELLO sits in the field at 100-190 by 120-150 points.
    let share = dark_share(&png, (209, 104, 396, 167));
    assert!(share > 0.02, "the field's value is drawn: {share}");
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn existing_pages_are_refused_and_y_replaces_only_them() {
    let dir = tempfile::tempdir().unwrap();
    let input = copy_sample(dir.path(), "report.pdf");
    let out = dir.path().join("report.png");
    let folder = dir.path().join("report");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("notes.txt"), b"mine").unwrap();
    std::fs::write(folder.join("report-2.png"), b"old").unwrap();

    let e = drawn_pages(&input, &out, Tuning::default(), false).unwrap_err();
    assert_eq!(e.code, ErrorCode::OutputExists);
    assert_eq!(
        e.message,
        format!(
            "{} exists; pass -y to overwrite",
            folder.join("report-2.png").display()
        )
    );
    assert!(!folder.join("report-1.png").exists(), "nothing was written");

    drawn_pages(&input, &out, Tuning::default(), true).unwrap();
    assert_eq!(std::fs::read(folder.join("notes.txt")).unwrap(), b"mine");
    assert_ne!(std::fs::read(folder.join("report-2.png")).unwrap(), b"old");
    assert!(folder.join("report-3.png").is_file());
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn a_locked_pdf_is_refused_as_password_protected() {
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("secret.pdf");
    let sample = sample();
    qpdf_ok(&[
        "--encrypt",
        "--user-password=open",
        "--owner-password=owner",
        "--bits=256",
        "--",
        s(&sample),
        s(&locked),
    ]);

    let e = drawn_pages(
        &locked,
        &dir.path().join("secret.png"),
        Tuning::default(),
        false,
    )
    .unwrap_err();

    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert_eq!(
        e.message,
        "secret.pdf is password-protected, and conv can't open it yet"
    );
}

#[test]
#[ignore = "requires qpdf and typst; run with --ignored"]
fn more_than_a_thousand_pages_needs_consent_and_writes_nothing_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let big = many_pages(dir.path(), 334); // 1,002 pages

    let e = drawn_pages(&big, &dir.path().join("doc.png"), Tuning::default(), false).unwrap_err();

    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert!(
        e.message.starts_with("saving 1,002 pages of "),
        "{}",
        e.message
    );
    assert!(!dir.path().join("doc").exists());
}

/// A destination close to Windows' path limit, with a `%` in it. There,
/// every path conv hands qpdf, Typst, ffmpeg and ImageMagick goes in
/// extended (`\\?\`) form, as `exec` hands every other step's (`winpath`),
/// and ffmpeg's pattern keeps that `%` a `%` once the rewriter has made it
/// absolute. Elsewhere nothing is long, and this checks the same run works.
#[test]
#[ignore = "requires qpdf, typst, ffmpeg and ImageMagick; run with --ignored"]
fn pages_are_saved_in_a_destination_that_approaches_max_path() {
    let base = tempfile::tempdir().unwrap();
    let mut deep = base.path().join("half_%_done");
    while deep.as_os_str().len() < 240 {
        deep = deep.join("convkit_deep_destination_directory_segment");
    }
    std::fs::create_dir_all(&deep).unwrap();
    let input = copy_sample(&deep, "report.pdf");
    assert!(
        input.as_os_str().len() >= 248,
        "the input must pass the verbatim threshold: {} ({} bytes)",
        input.display(),
        input.as_os_str().len()
    );

    for ext in ["jpg", "webp"] {
        let o = pages(
            &input,
            &deep.join(format!("report.{ext}")),
            Tuning::default(),
            false,
        )
        .unwrap_or_else(|e| {
            panic!(
                "pdf -> {ext} in a {}-character folder failed: {e}",
                deep.as_os_str().len()
            )
        });
        let r = report(&o);
        assert_eq!(r.files.len(), 3, "{ext}");
        for name in &r.files {
            let len = std::fs::metadata(o.output.join(name)).map_or(0, |m| m.len());
            assert!(len > 0, "{name} was not written");
        }
    }
}
