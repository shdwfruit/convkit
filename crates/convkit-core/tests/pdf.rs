//! `conv merge` / `conv split` against a real qpdf.
//!
//! `#[ignore]`-gated like `output_properties.rs`: `cargo test` stays green
//! without qpdf, and `cargo test -- --ignored` runs these wherever qpdf
//! resolves (PATH, `conv install qpdf`, or `CONVKIT_QPDF`).

use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::pdf::{self, parse_range, PdfOutcome};
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
    let plan = pdf::plan_merge(&q.path, inputs, output).unwrap();
    pdf::run(&plan, &q, overwrite, &mut |_| {}).unwrap()
}

fn split(input: &Path, ranges: &[&str], outdir: Option<&Path>) -> PdfOutcome {
    let q = qpdf();
    let ranges: Vec<_> = ranges.iter().map(|r| parse_range(r).unwrap()).collect();
    let plan = pdf::plan_split(&q.path, input, &ranges, outdir).unwrap();
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
    )
    .and_then(|plan| pdf::run(&plan, &q, false, &mut |_| {}));
    std::env::set_current_dir(cwd).unwrap();

    result.unwrap();
    assert_eq!(page_count(&out), 9);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_password_protected_pdf_is_refused() {
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

    let e =
        pdf::plan_merge(&qpdf().path, &[locked, sample], &dir.path().join("out.pdf")).unwrap_err();

    assert_eq!(e.code, ErrorCode::ConversionFailed);
    assert_eq!(
        e.message,
        "secret.pdf is password-protected, and conv can't open it yet"
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
    let plan = pdf::plan_split(&q.path, &input, &ranges, None).unwrap();

    let e = pdf::run(&plan, &q, false, &mut |_| {}).unwrap_err();

    assert_eq!(e.code, ErrorCode::OutputExists);
    assert!(!dir.path().join("sample-1.pdf").exists());
}
