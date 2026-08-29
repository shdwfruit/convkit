use assert_cmd::Command;
use predicates::str::contains;

fn conv() -> Command {
    Command::cargo_bin("conv").unwrap()
}

#[test]
fn dry_run_prints_the_expert_ffmpeg_command() {
    conv()
        .args(["in.mp4", "out.gif", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("ffmpeg"))
        .stdout(contains("palettegen=stats_mode=diff"));
}

#[test]
fn unknown_extension_exits_two_and_suggests() {
    conv()
        .args(["in.mp4", "out.gff", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("did you mean"));
}

#[test]
fn unsupported_pair_exits_two() {
    conv()
        .args(["in.pdf", "out.mp4", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("not supported"));
}

#[test]
fn dot_extension_form_derives_the_output_name() {
    conv()
        .args(["photo.heic", ".jpg", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("photo.jpg"));
}

#[test]
fn json_dry_run_reports_ok_and_the_first_step_program() {
    let assert = conv()
        .args(["in.mp4", "out.gif", "--dry-run", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout must be valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["plan"]["steps"][0]["program"], "ffmpeg");
}

#[test]
fn json_error_envelope_lands_on_stderr_for_an_unsupported_pair() {
    let assert = conv()
        .args(["in.pdf", "out.mp4", "--dry-run", "--json"])
        .assert()
        .code(2);
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stderr).expect("stderr must be valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unsupported_pair");
}

#[test]
fn capabilities_json_lists_pairs_with_their_backends() {
    let out = conv().args(["capabilities", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pairs = v["pairs"].as_array().unwrap();
    assert!(
        pairs.len() > 30,
        "expected the full table, got {}",
        pairs.len()
    );
    let gif = pairs
        .iter()
        .find(|p| p["from"] == "mp4" && p["to"] == "gif")
        .unwrap();
    assert_eq!(gif["backends"][0], "ffmpeg");
}

#[test]
fn doctor_reports_every_backend_and_never_exits_nonzero_for_missing_ones() {
    conv().arg("doctor").assert().success();
}

#[test]
fn doctor_json_marks_libreoffice_as_manual_install_only() {
    let out = conv().args(["doctor", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let lo = v["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["backend"] == "soffice")
        .unwrap();
    assert_eq!(lo["managed_install"], false);
}

// --- Controller review round 3 -------------------------------------------

/// A real (non-dry-run) failing conversion must report on stderr, never
/// stdout — a script piping stdout to a file and watching stderr for
/// trouble (`2>errors.log`) must see nothing on success's stream when a job
/// fails. Uses an already-existing output with no `-y` so the failure is
/// deterministic and needs no real backend (the `OutputExists` check runs
/// before any backend is resolved).
#[test]
fn a_real_failing_conversion_writes_nothing_to_stdout_and_reports_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("in.mp4"), b"input").unwrap();
    std::fs::write(dir.path().join("out.gif"), b"already here").unwrap();

    let assert = conv()
        .current_dir(dir.path())
        .args(["in.mp4", "out.gif"])
        .assert()
        .code(2);
    let output = assert.get_output();
    assert_eq!(
        output.stdout,
        b"",
        "a failing conversion must not write to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exists"), "{stderr}");
}

/// `--dry-run` must not abort the whole preview on the first job whose plan
/// fails to build — a bad job among several others shouldn't erase the
/// preview for the rest, the same way a real run tolerates one job failing
/// without losing the others' results. `a.heic -> jpg` has a recipe;
/// `b.pdf -> jpg` does not (pandoc/soffice don't read PDF into an image), so
/// this is a genuine mixed result: exit 4, the first job's plan on stdout,
/// the second job's error on stderr.
#[test]
fn dry_run_previews_every_job_even_when_one_fails_to_build() {
    let assert = conv()
        .args(["a.heic", "b.pdf", "--to", "jpg", "--dry-run"])
        .assert()
        .code(4);
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("magick"), "{stdout}");
    assert!(stderr.contains("not supported"), "{stderr}");
}

/// When every job in a multi-job `--dry-run` fails, the exit code must be
/// the underlying error's own code (here `unsupported_pair`, 2) — not the
/// generic partial-failure code (4), which is reserved for a genuinely
/// mixed result. Mirrors `batch::run`'s real-execution rule.
#[test]
fn dry_run_exits_with_the_underlying_code_when_every_job_fails() {
    conv()
        .args(["a.pdf", "b.docx", "--to", "mp4", "--dry-run"])
        .assert()
        .code(2);
}

// --- Task 14: `conv install` --------------------------------------------
//
// Only the no-network refusal paths are covered here — a real download is
// exercised by the task's end-to-end acceptance check, not by a test that
// would make `cargo test --workspace` depend on network access.

/// LibreOffice has no relocatable binary, so `conv install soffice` must
/// refuse outright — never print a "downloading" line, never attempt a
/// fetch — and point at the manual install command instead.
#[test]
fn install_soffice_refuses_without_attempting_a_download() {
    let assert = conv().args(["install", "soffice"]).assert().code(3);
    let output = assert.get_output();
    assert_eq!(
        output.stdout,
        b"",
        "must not print anything on stdout, especially not a download line: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.to_ascii_lowercase().contains("downloading"),
        "{stderr}"
    );
    assert!(stderr.contains("soffice"), "{stderr}");
    assert!(
        stderr.contains("or:"),
        "must still offer a manual hint: {stderr}"
    );
    assert!(
        !stderr.contains("try:"),
        "must not offer `conv install soffice` as its own fix: {stderr}"
    );
}

/// The `--json` form of the same refusal: still no network attempt, and the
/// error envelope's `remediation.managed` is `null`.
#[test]
fn install_soffice_json_refusal_has_no_managed_remediation() {
    let assert = conv()
        .args(["install", "soffice", "--json"])
        .assert()
        .code(3);
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stderr).expect("stderr must be valid JSON");
    assert_eq!(v["ok"], false);
    assert!(v["error"]["remediation"]["managed"].is_null());
    assert!(v["error"]["remediation"]["manual"].is_string());
}

/// A backend name this CLI doesn't recognise at all is a malformed
/// invocation (exit 2), distinct from a recognised-but-refused backend
/// (exit 3).
#[test]
fn install_rejects_an_unrecognised_backend_name() {
    conv()
        .args(["install", "not-a-real-backend"])
        .assert()
        .code(2)
        .stderr(contains("unknown backend"));
}

/// `magick` is `is_managed()` (unlike `soffice`), but this task's manifest
/// verifies no ImageMagick asset on any platform (every official release is
/// `.7z` on Windows, an AppImage on Linux, or has no portable macOS build at
/// all) — so on every platform this test runs on, `conv install magick`
/// must report the missing-manifest-entry refusal, not attempt a download.
#[test]
fn install_magick_reports_no_managed_build_on_every_platform() {
    let assert = conv().args(["install", "magick"]).assert().code(3);
    let output = assert.get_output();
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no managed build"), "{stderr}");
    assert!(
        !stderr.to_ascii_lowercase().contains("downloading"),
        "{stderr}"
    );
}
