use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
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

/// I2: `--dry-run --json` always uses the plural `"plans"` array, even for
/// a single job — one of the `--json` contract's four previously
/// incompatible success shapes (a lone job used to get its own singular
/// `"plan"` key instead), so a consumer no longer has to branch on job
/// count to find the plan.
#[test]
fn json_dry_run_reports_ok_and_the_first_step_program() {
    let assert = conv()
        .args(["in.mp4", "out.gif", "--dry-run", "--json"])
        .assert()
        .success();
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout must be valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["plans"][0]["ok"], true);
    assert_eq!(v["plans"][0]["plan"]["steps"][0]["program"], "ffmpeg");
}

/// I2: a per-job plan-build failure (the job itself was well-formed —
/// `in.pdf out.mp4` parses fine — but no recipe exists for the pair) is
/// reported the same way a batch of many jobs already reported one bad job
/// among them: inside the `"plans"` array on stdout, `ok: false`, never on
/// stderr. This is the counterpart to the next test, which covers the
/// genuine "no job could even be built" case that still belongs on stderr.
#[test]
fn json_dry_run_reports_a_per_job_plan_failure_inside_the_plans_array_not_stderr() {
    let assert = conv()
        .args(["in.pdf", "out.mp4", "--dry-run", "--json"])
        .assert()
        .code(2);
    let output = assert.get_output();
    assert_eq!(
        output.stderr,
        b"",
        "a per-job failure belongs in the stdout array, not stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["plans"][0]["ok"], false);
    assert_eq!(v["plans"][0]["error"]["code"], "unsupported_pair");
}

/// The genuine top-level case — no job exists at all, because the
/// invocation itself doesn't parse (here: no positionals given at all) —
/// still gets its own top-level `{"ok": false, "error": ...}` document on
/// stderr, per the README's documented contract ("only an error caught
/// before any job exists at all ... is its own top-level document on
/// stderr instead").
#[test]
fn json_error_envelope_lands_on_stderr_when_no_job_could_be_built_at_all() {
    let assert = conv().args(["--dry-run", "--json"]).assert().code(2);
    let v: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stderr).expect("stderr must be valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "invalid_invocation");
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

/// C1: `magick` is `is_managed() == true` (a managed install is
/// architecturally possible in principle) but has zero verified manifest
/// entries on any platform — so `doctor --json`'s `managed_install` field
/// must report `false` for it too, the same as `soffice`, regardless of
/// whether this machine happens to have a real ImageMagick installed
/// (`found` may be `true` or `false`; `managed_install` must be `false`
/// either way, since it answers "would `conv install magick` work here",
/// not "is magick currently found").
#[test]
fn doctor_json_marks_imagemagick_as_manual_install_only() {
    let out = conv().args(["doctor", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let magick = v["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["backend"] == "magick")
        .unwrap();
    assert_eq!(magick["managed_install"], false);
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

// --- Task 2: docx/odt -> pdf availability-based recipe selection --------
//
// A previous review found `--dry-run` printing a transcode command for a
// run that would actually stream-copy; the fix there was to have dry-run
// probe (see `commands/convert.rs`'s `probed_for`). The same lesson applies
// here: `--dry-run` must preview the pandoc+typst fallback command when
// soffice is unavailable, not the (unusable) soffice one. `--soffice-path`
// is pointed at a file guaranteed not to exist — the same per-backend
// override mechanism `cli.rs`'s `Cli::resolver` already wires up for every
// backend — but on Windows/macOS a plain nonexistent override alone falls
// through to `WellKnown`, the only candidate a standard LibreOffice install
// is ever found through there (its installer doesn't add `program\`/
// `MacOS/` to `PATH`) — so once this project's own dev machine got a real,
// working LibreOffice install, `--soffice-path <missing>` alone stopped
// being enough to make soffice unresolvable. `CONVKIT_NO_WELL_KNOWN` is
// `Resolver`'s escape hatch for exactly this (see its docs in
// `resolve.rs`): safe to set here specifically because `assert_cmd`'s
// `.env(...)` only affects this one child process's environment, never the
// test binary's own, so it can't leak into any other test running
// concurrently in this suite.
#[test]
fn dry_run_previews_the_pandoc_typst_fallback_when_soffice_path_is_unresolvable() {
    let dir = tempfile::tempdir().unwrap();
    // Any real, existing file resolves successfully — `Resolver::resolve`
    // only requires `path.is_file()` plus a version probe that degrades to
    // "unknown" on failure rather than erroring, so these don't need to be
    // real pandoc/typst binaries.
    let pandoc_stub = dir.path().join("pandoc_stub");
    std::fs::write(&pandoc_stub, b"stub").unwrap();
    let typst_stub = dir.path().join("typst_stub");
    std::fs::write(&typst_stub, b"stub").unwrap();
    let missing_soffice = dir.path().join("definitely-does-not-exist");

    conv()
        .env("CONVKIT_NO_WELL_KNOWN", "1")
        .args(["in.docx", "out.pdf", "--dry-run"])
        .arg("--pandoc-path")
        .arg(&pandoc_stub)
        .arg("--typst-path")
        .arg(&typst_stub)
        .arg("--soffice-path")
        .arg(&missing_soffice)
        .assert()
        .success()
        .stdout(contains("pandoc"))
        .stdout(contains("--pdf-engine"))
        .stdout(contains("typst"))
        .stdout(contains("soffice").not())
        .stdout(contains("Install LibreOffice for higher fidelity"));
}

/// The mirror image: with soffice genuinely resolvable, `--dry-run` must
/// keep previewing the canonical soffice command even though pandoc and
/// typst are also both available — soffice wins whenever it's present.
#[test]
fn dry_run_still_previews_soffice_when_it_is_available_even_alongside_pandoc_and_typst() {
    let dir = tempfile::tempdir().unwrap();
    let soffice_stub = dir.path().join("soffice_stub");
    std::fs::write(&soffice_stub, b"stub").unwrap();
    let pandoc_stub = dir.path().join("pandoc_stub");
    std::fs::write(&pandoc_stub, b"stub").unwrap();
    let typst_stub = dir.path().join("typst_stub");
    std::fs::write(&typst_stub, b"stub").unwrap();

    conv()
        .args(["in.docx", "out.pdf", "--dry-run"])
        .arg("--soffice-path")
        .arg(&soffice_stub)
        .arg("--pandoc-path")
        .arg(&pandoc_stub)
        .arg("--typst-path")
        .arg(&typst_stub)
        .assert()
        .success()
        .stdout(contains("soffice"))
        .stdout(contains("--convert-to"))
        .stdout(contains("--pdf-engine").not());
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

// --- Part 1: install-and-retry prompt for a missing backend ----------------
//
// `magick` is the reliably-missing backend used below: this project's own
// bare CI runner has no conversion backends installed at all, and `magick`
// specifically is also absent on the machine this task was developed
// against (ImageMagick is installed but not linked onto PATH) — unlike
// `ffmpeg`/`pandoc`/`typst`, which a prior `conv install` had already
// provisioned as managed backends on that same machine, making them
// unsuitable for a deterministic "still missing" test here. `magick` also
// happens to never be offerable (`manifest::has_managed_build` is `false`
// for it on every platform — no verified manifest entry exists), so these
// prove "no prompt, no hang" via that gate; the Windows-only test further
// down proves the same guarantee for a genuinely offerable backend, via the
// TTY gate specifically.
//
// Every test here adds an explicit `.timeout(...)` on top of assert_cmd's
// own default (stdin is always a pipe, never a real TTY, so
// `install_prompt::is_interactive_session()` should never even try to
// read) — a hard backstop so a regression here fails loudly instead of
// hanging the suite.

fn write_unreadable_png(dir: &std::path::Path) {
    std::fs::write(
        dir.join("a.png"),
        b"not a real png, but never read by magick",
    )
    .unwrap();
}

#[test]
fn piped_stdin_never_prompts_and_reports_the_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let output = assert.get_output();
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("magick"), "{stderr}");
    assert!(
        !stderr.contains("Install it now"),
        "must never prompt when stdin is piped: {stderr}"
    );
}

/// Acceptance check 4: `--json` on a missing-backend case is unchanged and
/// exits 3 with no prompt.
#[test]
fn json_mode_never_prompts_on_a_missing_backend() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--json"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let output = assert.get_output();
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("Install it now"), "{stdout_text}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Install it now"), "{stderr}");
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["results"][0]["error"]["code"], "backend_missing");
}

#[test]
fn no_install_flag_never_prompts_even_though_it_would_otherwise_be_offerable() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--no-install"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(!stderr.contains("Install it now"), "{stderr}");
    assert!(
        !stderr.to_ascii_lowercase().contains("downloading"),
        "{stderr}"
    );
}

#[test]
fn quiet_flag_never_prompts_either() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--quiet"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3)
        .stderr(contains("Install it now").not());
}

#[test]
fn yes_and_no_install_together_is_a_usage_error() {
    conv()
        .args(["in.mp4", "out.gif", "--yes", "--no-install"])
        .assert()
        .code(2)
        .stderr(contains("cannot be used with"));
}

/// The strongest version of the non-interactive guarantee: even a backend
/// that genuinely *is* offerable (`manifest::has_managed_build` is `true`
/// for `ffmpeg` on this platform) must never prompt when stdin is piped —
/// proving the TTY gate itself, not just the "never offerable at all" gate
/// every test above exercises via `magick`.
///
/// `ffmpeg` is already provisioned as a managed backend on the machine this
/// was developed against (a prior `conv install ffmpeg` put it in the real,
/// shared `%LOCALAPPDATA%\convkit\bin`), so a plain `--ffmpeg-path
/// <nonexistent>` alone doesn't make it unresolvable — `Resolver::resolve`
/// falls through an unusable override to the next candidate, and Managed
/// still finds the real one. This redirects `LOCALAPPDATA` to an empty
/// temp directory and narrows `PATH` to just `System32`, for this child
/// process only — never touching the real managed install, or anything
/// else on the host — so `ffmpeg` genuinely can't be found for this one
/// invocation regardless of what's actually installed.
#[cfg(windows)]
#[test]
fn piped_stdin_never_prompts_even_for_a_genuinely_offerable_backend() {
    let dir = tempfile::tempdir().unwrap();
    let fake_local_app_data = dir.path().join("fake-local-app-data");
    std::fs::create_dir_all(&fake_local_app_data).unwrap();
    std::fs::write(dir.path().join("in.mp4"), b"not a real mp4").unwrap();

    let assert = conv()
        .current_dir(dir.path())
        .env("LOCALAPPDATA", &fake_local_app_data)
        .env("PATH", r"C:\Windows\System32")
        .args(["in.mp4", "out.gif"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let output = assert.get_output();
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ffmpeg not found"), "{stderr}");
    assert!(
        !stderr.contains("Install it now"),
        "must never prompt when stdin is piped, even for an offerable backend: {stderr}"
    );
}

// --- Part 2: informative success/failure/batch output -----------------------

/// Writes a script standing in for `magick`: on a bare version probe
/// (`Resolver::resolve`'s own check) it no-ops and exits 0; otherwise it
/// writes one byte to whatever its last argument names. Lets these tests
/// exercise the real success-rendering path without depending on whether a
/// real ImageMagick happens to be on this machine's PATH.
fn write_magick_stub(dir: &std::path::Path) -> std::path::PathBuf {
    let (name, body) = if cfg!(windows) {
        (
            "magick_stub.bat",
            "@echo off\r\n\
             if not \"%~2\"==\"\" goto notversion\r\n\
             if \"%~1\"==\"--version\" exit /b 0\r\n\
             if \"%~1\"==\"-version\" exit /b 0\r\n\
             :notversion\r\n\
             :loop\r\n\
             if \"%~1\"==\"\" goto done\r\n\
             set \"last=%~1\"\r\n\
             shift\r\n\
             goto loop\r\n\
             :done\r\n\
             <nul set /p \"=x\" >\"%last%\"\r\n\
             exit /b 0\r\n",
        )
    } else {
        (
            "magick_stub.sh",
            "#!/bin/sh\n\
             if [ \"$#\" = \"1\" ] && { [ \"$1\" = \"--version\" ] || [ \"$1\" = \"-version\" ]; }; then\n\
             \x20   exit 0\n\
             fi\n\
             for a in \"$@\"; do last=\"$a\"; done\n\
             printf x > \"$last\"\n",
        )
    };
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

/// Acceptance check 1's shape, proven with a stub so it never depends on a
/// real ImageMagick: success prints a result line with a size and elapsed
/// time, then — always, per the owner's own complaint — the absolute,
/// resolved output path on its own line.
#[test]
fn successful_conversion_prints_result_line_and_the_absolute_output_path() {
    let dir = tempfile::tempdir().unwrap();
    let stub = write_magick_stub(dir.path());
    std::fs::write(dir.path().join("a.png"), b"x").unwrap();

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--magick-path"])
        .arg(&stub)
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.stderr, b"", "{:?}", output.stderr);
    // assert_cmd always gives the child a piped (non-tty) stdout, so this
    // also proves the acceptance check's "piped through cat" case: plain
    // ASCII, no escape codes.
    assert!(!stdout.contains('\u{1b}'), "{stdout}");
    assert!(stdout.starts_with("OK "), "{stdout}");
    let abs = std::path::absolute(dir.path().join("a.jpg")).unwrap();
    assert!(stdout.contains(&abs.display().to_string()), "{stdout}");
}

#[test]
fn quiet_suppresses_success_output_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let stub = write_magick_stub(dir.path());
    std::fs::write(dir.path().join("a.png"), b"x").unwrap();

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--quiet", "--magick-path"])
        .arg(&stub)
        .assert()
        .success();
    let output = assert.get_output();
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
}

/// `--json` success is unaffected by Part 2's human-mode redesign except
/// for the additive `elapsed_ms` key.
#[test]
fn json_success_is_unchanged_except_for_the_additive_elapsed_ms_key() {
    let dir = tempfile::tempdir().unwrap();
    let stub = write_magick_stub(dir.path());
    std::fs::write(dir.path().join("a.png"), b"x").unwrap();

    let out = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg", "--json", "--magick-path"])
        .arg(&stub)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["results"][0]["ok"], true);
    assert!(v["results"][0]["elapsed_ms"].is_number(), "{v}");
}

/// Acceptance check 3's shape: a real, unstubbed failing conversion (no
/// ImageMagick on this machine's PATH — see this module's own doc comment
/// on why `magick` is the reliable choice) must not look like a success.
#[test]
fn failing_conversion_prints_a_fail_header_message_and_one_remediation_line() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let assert = conv()
        .current_dir(dir.path())
        .args(["a.png", "a.jpg"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let output = assert.get_output();
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
    assert!(stderr.starts_with("FAIL "), "{stderr}");
    assert!(stderr.contains("a.png"), "{stderr}");
    assert!(stderr.contains("jpg"), "{stderr}");
    assert!(stderr.contains("magick not found"), "{stderr}");
    assert!(stderr.contains("try"), "{stderr}");
}
