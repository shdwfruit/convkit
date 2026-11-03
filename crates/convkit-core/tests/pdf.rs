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
