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
// soffice is unavailable, not the (unusable) soffice one. Soffice gets no
// `--soffice-path` at all (an earlier version of this test pointed one at a
// file guaranteed not to exist, relying on `Resolver::resolve` skipping a
// candidate whose path isn't a file and falling through to the next one —
// but since the override-authority fix, a present-but-nonexistent
// `--soffice-path` is now a hard, immediate `InvalidInvocation` error
// rather than a skipped candidate, see `resolve.rs`'s `Resolver::resolve`
// docs, so that would now make this test about a bad flag value instead of
// about soffice genuinely being unavailable). `command_with_no_backends`
// (see its own docs above) closes every candidate `Resolver::candidates`
// would otherwise try for a backend with no override — `Source::Env`'s
// `CONVKIT_SOFFICE`, a real `soffice` on `PATH`, and, on Windows/macOS only,
// `Source::WellKnown`'s fixed Program Files/`/Applications` locations — for
// this one child process, without touching this test binary's own
// environment or any other test running concurrently in this suite; pandoc
// and typst still resolve regardless, since their `--pandoc-path`/
// `--typst-path` overrides point at real, existing stub files and
// `Resolver::resolve` returns the very first candidate that's a file,
// before ever consulting env/managed/PATH/well-known for them.
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

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    cmd.args(["in.docx", "out.pdf", "--dry-run"])
        .arg("--pandoc-path")
        .arg(&pandoc_stub)
        .arg("--typst-path")
        .arg(&typst_stub)
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
// `magick` is the backend used below. It's never offerable
// (`manifest::has_managed_build` is `false` for it on every platform — no
// verified manifest entry exists), so these prove "no prompt, no hang" via
// that gate; the Windows-only test further down proves the same guarantee
// for a genuinely offerable backend, via the TTY gate specifically.
//
// Every test here that expects `backend_missing` needs `magick` to
// genuinely fail to resolve — and *only* relying on this host happening to
// lack a real ImageMagick install is exactly the bug a prior version of
// this suite had: GitHub's `windows-latest` runner ships ImageMagick
// pre-installed, so `magick` resolved there, ran, and correctly rejected
// the deliberately-invalid `a.png` with `conversion_failed` (exit 1) instead
// of `backend_missing` (exit 3) — the same disease an earlier review had
// already found and fixed the other way around (tests that failed on a
// machine *with* pandoc or LibreOffice installed). `command_with_no_backends`
// below closes every candidate `Resolver::candidates` would otherwise try,
// so these tests exercise the `backend_missing` path deterministically
// regardless of what's actually installed on the host running them.
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

/// Gives a `conv` child process an environment in which *no* backend can
/// possibly resolve, regardless of what happens to be installed, on `PATH`,
/// or set in `CONVKIT_*` on the host actually running this suite —
/// `Resolver::candidates`'s full chain is explicit `--<backend>-path` flag
/// -> `CONVKIT_<BACKEND>` env var -> managed directory -> `PATH` ->
/// well-known platform locations, and every one of those but the first
/// (which no test here ever passes) is closed off:
///
/// - `env_clear()` drops the whole inherited environment first, including
///   every `CONVKIT_*` override a developer's own shell might happen to
///   have set — a plain `.env(...)` added on top of the inherited
///   environment could never undo that, only add to it.
/// - `PATH` is then set to a fresh, empty tempdir rather than left unset or
///   cleared to nothing: an empty-string `PATH` still has one path
///   component — `""` — which conventionally means "the current
///   directory", not "no directories", and every test here genuinely runs
///   with a real current directory. A real, empty directory has no such
///   loophole.
/// - the managed-install directory (`LOCALAPPDATA` on Windows,
///   `XDG_DATA_HOME` elsewhere — see `Resolver::managed_dir`) is pointed at
///   a second fresh, empty tempdir, so `Source::Managed` can't find a real
///   `conv install`-placed binary either.
/// - `CONVKIT_NO_WELL_KNOWN` is set, `Resolver`'s own escape hatch (see
///   `resolve.rs`) for the one candidate an emptied `PATH` can't touch:
///   `Source::WellKnown`'s fixed absolute install locations (LibreOffice's
///   Windows/macOS paths are the only ones any backend has). This is a
///   no-op for `magick`, which has no well-known locations on any platform,
///   but keeps this helper correct for any other backend a test might use
///   it with.
/// - `SYSTEMROOT` is added back from this test process's own real
///   environment, when present — verified empirically (not assumed) to be
///   unnecessary for `conv.exe` to spawn at all in this environment, but
///   cheap insurance against a Windows process that needs it to start on a
///   host where it matters, and it can never help a backend resolve.
///
/// Returns the two `TempDir` guards alongside `cmd` so callers keep them
/// alive for as long as the assertion needs them — they delete their
/// directory on drop, and `cmd`'s env values are borrowed paths into them.
fn command_with_no_backends() -> (Command, tempfile::TempDir, tempfile::TempDir) {
    let empty_path = tempfile::tempdir().unwrap();
    let empty_managed_dir = tempfile::tempdir().unwrap();

    let mut cmd = conv();
    cmd.env_clear();
    if let Ok(system_root) = std::env::var("SYSTEMROOT") {
        cmd.env("SYSTEMROOT", system_root);
    }
    cmd.env("PATH", empty_path.path());
    #[cfg(windows)]
    cmd.env("LOCALAPPDATA", empty_managed_dir.path());
    #[cfg(not(windows))]
    cmd.env("XDG_DATA_HOME", empty_managed_dir.path());
    cmd.env("CONVKIT_NO_WELL_KNOWN", "1");

    (cmd, empty_path, empty_managed_dir)
}

#[test]
fn piped_stdin_never_prompts_and_reports_the_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
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

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
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

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
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

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    cmd.current_dir(dir.path())
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
/// `ffmpeg` is a poor fit for `command_with_no_backends`'s usual well: a
/// plain `--ffmpeg-path <nonexistent>` doesn't exercise the scenario this
/// test is actually about either way. Before the override-authority fix, it
/// wouldn't have made ffmpeg unresolvable at all on a machine where a prior
/// `conv install ffmpeg` already provisioned it as a managed backend
/// (`Resolver::resolve` fell through an unusable override to the next
/// candidate, and `Source::Managed` still found the real one). Since that
/// fix (see `resolve.rs`'s `Resolver::resolve` docs), it would make ffmpeg
/// fail even more readily — but with a hard, immediate `InvalidInvocation`
/// (exit 2) naming the bad `--ffmpeg-path`, not the `backend_missing`
/// (exit 3) this test needs to prove the TTY gate against. Either way, this
/// is exactly why `command_with_no_backends` closes every *other* candidate
/// instead: it redirects the managed directory, not just `PATH`, and clears
/// every `CONVKIT_*` var via `env_clear()` (a `CONVKIT_FFMPEG` set in a
/// developer's own shell would otherwise resolve here too, via
/// `Source::Env`, ahead of both). No `--ffmpeg-path` override is passed at
/// all: the whole point is that `ffmpeg` fails to resolve through every
/// candidate genuinely, the same `backend_missing` way a machine with no
/// ffmpeg installed anywhere would see.
#[cfg(windows)]
#[test]
fn piped_stdin_never_prompts_even_for_a_genuinely_offerable_backend() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("in.mp4"), b"not a real mp4").unwrap();

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
        .current_dir(dir.path())
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

/// Acceptance check 3's shape: a real, unstubbed failing conversion (backend
/// deliberately made unresolvable via `command_with_no_backends` — see this
/// module's own doc comment on why `magick` is the reliable choice) must not
/// look like a success.
#[test]
fn failing_conversion_prints_a_fail_header_message_and_one_remediation_line() {
    let dir = tempfile::tempdir().unwrap();
    write_unreadable_png(dir.path());

    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
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

// --- `conv update` / `conv update --check` --------------------------------
//
// Only `--check` is exercised here — it changes nothing and touches no
// network, the same reasoning `install`'s own tests above give for covering
// just the no-network refusal paths rather than a real download.
// `command_with_no_backends` (see its own docs above) makes every backend,
// managed and unmanaged alike, deterministically unresolvable regardless of
// what this host actually has installed, so these are green in both the
// clean CI environment and the project's own hostile one (real ImageMagick
// and LibreOffice on `PATH`) — that host state only ever changes the
// unmanaged rows, never whether a managed backend is installed.
//
// Review finding F42 changed what "every backend absent" means: a managed
// backend nobody has ever installed is `"not_installed"`, not `"missing"`,
// and (unlike the old `"missing"`) never fails `--check` on its own -- see
// `commands::update::ok`'s own docs. The tests below were rewritten around
// that; the genuinely-failing case now needs an actually *outdated*
// managed backend, exercised separately below.

/// Acceptance check 1/2's shape, made host-independent: with every backend
/// unresolvable, `--check` must name every managed backend as not
/// installed (not silently skip any), report the two unmanaged backends
/// without ever running a package manager, and — since nothing here is
/// genuinely broken, just never provisioned (review finding F42) — exit
/// zero.
#[test]
fn update_check_reports_every_managed_backend_as_not_installed_in_an_isolated_environment() {
    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
        .args(["update", "--check"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    for name in ["ffmpeg", "ffprobe", "pandoc", "typst"] {
        assert!(stdout.contains(name), "{stdout}");
    }
    assert!(stdout.contains("not installed"), "{stdout}");
    assert!(
        stdout.contains("conv install"),
        "must point at `conv install <backend>` for provisioning: {stdout}"
    );
    assert!(stdout.contains("magick"), "{stdout}");
    assert!(stdout.contains("soffice"), "{stdout}");
    assert!(stdout.contains("unmanaged"), "{stdout}");
    assert!(
        !stdout.to_ascii_lowercase().contains("downloading"),
        "a never-installed backend must never trigger a download: {stdout}"
    );
    assert!(
        stdout.contains("conv "),
        "must report conv's own version: {stdout}"
    );
}

/// The `--json` shape of the same scenario: every managed backend reports
/// `"not_installed"` (never the old `"missing"`), and the envelope is
/// `ok: true` on stdout -- review finding F42's whole point is that this
/// state is informational, not a failure.
#[test]
fn update_check_json_reports_a_never_installed_backend_as_not_installed_and_ok() {
    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
        .args(["update", "--check", "--json"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(0);
    let output = assert.get_output();
    assert_eq!(
        output.stderr,
        b"",
        "an ok envelope must land on stdout, not stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(v["ok"], true);
    let backends = v["backends"].as_array().expect("backends must be an array");
    assert_eq!(backends.len(), 6, "{v}");
    let ffmpeg = backends
        .iter()
        .find(|b| b["backend"] == "ffmpeg")
        .expect("ffmpeg must be reported");
    assert_eq!(ffmpeg["action"], "not_installed");
    assert_eq!(ffmpeg["managed"], true);
    let magick = backends
        .iter()
        .find(|b| b["backend"] == "magick")
        .expect("magick must be reported");
    assert_eq!(magick["action"], "unmanaged");
    assert_eq!(magick["managed"], false);
    assert!(magick["manual_hint"].is_string(), "{v}");
    assert!(v["conv"]["version"].is_string(), "{v}");
    assert!(v["conv"]["update_hint"].is_string(), "{v}");
}

/// Acceptance check 4's shape, now built on a genuinely *outdated* managed
/// backend rather than merely a never-installed one: `--json`'s envelope
/// carries `ok`, the plural `backends` key, and an additive `conv` object
/// — and, mirroring `doctor`/`install`'s own documented stdout/stderr
/// split, a non-ok envelope lands on stderr, not stdout. Unix-only:
/// writing a real, executable stub directly at the exact filename
/// `Source::Managed` expects (`Resolver::managed_filename`) only works
/// cross-platform via `Source::Override`, which accepts any filename --
/// see `resolve::tests::resolve_managed_only_finds_a_file_written_at_the_
/// managed_path`'s own docs in `convkit-core` for the identical
/// constraint (a `.exe`-named text file fails to spawn on Windows at all).
#[cfg(unix)]
#[test]
fn update_check_json_envelope_lands_on_stderr_when_a_managed_backend_is_outdated() {
    let (mut cmd, _empty_path, managed_dir) = command_with_no_backends();

    // `Resolver::managed_dir()` joins `convkit/bin` onto `XDG_DATA_HOME`
    // (`command_with_no_backends` points that env var at `managed_dir`
    // directly) -- the stub must land at that exact real path, not at
    // `managed_dir`'s own root.
    let managed_bin_dir = managed_dir.path().join("convkit").join("bin");
    std::fs::create_dir_all(&managed_bin_dir).unwrap();
    let stub_path = managed_bin_dir.join("typst");
    std::fs::write(
        &stub_path,
        "#!/bin/sh\necho \"typst 0.0.1-not-the-pinned-version\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let assert = cmd
        .args(["update", "--check", "--json"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(3);
    let output = assert.get_output();
    assert_eq!(
        output.stdout,
        b"",
        "a non-ok envelope must land on stderr, not stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr must be valid JSON");
    assert_eq!(v["ok"], false);
    let backends = v["backends"].as_array().expect("backends must be an array");
    assert_eq!(backends.len(), 6, "{v}");
    let typst = backends
        .iter()
        .find(|b| b["backend"] == "typst")
        .expect("typst must be reported");
    assert_eq!(typst["action"], "outdated", "{v}");
    assert_eq!(typst["managed"], true);
    let ffmpeg = backends
        .iter()
        .find(|b| b["backend"] == "ffmpeg")
        .expect("ffmpeg must be reported");
    assert_eq!(
        ffmpeg["action"], "not_installed",
        "a sibling backend that's simply absent must not also be flagged: {v}"
    );
    let magick = backends
        .iter()
        .find(|b| b["backend"] == "magick")
        .expect("magick must be reported");
    assert_eq!(magick["action"], "unmanaged");
    assert_eq!(magick["managed"], false);
    assert!(magick["manual_hint"].is_string(), "{v}");
    assert!(v["conv"]["version"].is_string(), "{v}");
    assert!(v["conv"]["update_hint"].is_string(), "{v}");
}

/// `--no-install` must make `conv update` behave like `--check` — report
/// only, install nothing — the same "never install anything" meaning it
/// already carries for a real conversion's install-and-retry prompt.
#[test]
fn update_no_install_behaves_like_check_and_never_downloads() {
    let (mut cmd, _empty_path, _empty_managed_dir) = command_with_no_backends();
    let assert = cmd
        .args(["update", "--no-install"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !stdout.to_ascii_lowercase().contains("downloading"),
        "{stdout}"
    );
    assert!(stdout.contains("not installed"), "{stdout}");
}

/// The top-level `conv --help` command list must mention `update` in a
/// one-line description consistent in tone with the other subcommands —
/// a directory entry, not the full explanation (that lives in `conv update
/// --help`, covered below).
#[test]
fn top_level_help_lists_the_update_subcommand() {
    conv()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("update"))
        .stdout(contains("Update managed backends"));
}

/// `conv update --help` is this command's real documentation: it must
/// explain what "up to date" means (pinned/verified, not latest upstream),
/// state the consequence (updating conv itself advances the pins), be
/// explicit that it never replaces the running binary, and cover
/// `--check`. Wording is asserted loosely (substrings of the actual text)
/// so this doesn't lock the exact prose, only that each point is present.
#[test]
fn update_help_explains_the_pinned_not_latest_design_and_no_self_replace() {
    let out = conv().args(["update", "--help"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(stdout.contains("pinned"), "{stdout}");
    assert!(stdout.contains("upstream"), "{stdout}");
    assert!(
        stdout.contains("checksum") || stdout.contains("sha-256"),
        "{stdout}"
    );
    assert!(
        stdout.contains("advances the pins") || stdout.contains("advance the pins"),
        "{stdout}"
    );
    assert!(
        stdout.contains("never replaces") || stdout.contains("never replace"),
        "{stdout}"
    );
    assert!(stdout.contains("security surface"), "{stdout}");
    assert!(stdout.contains("--check"), "{stdout}");
    assert!(
        stdout.contains("path"),
        "must mention resolving by path (no shell restart needed): {stdout}"
    );
}

// --- CLI help papercut: conversion-only flags must not leak into every
// subcommand's --help -------------------------------------------------------
//
// `--dry-run`, `-y/--overwrite`, `-o/--outdir`, and `-j/--jobs` only mean
// something for the implicit conversion path (no subcommand) -- `conv
// update --outdir` is meaningless. They used to be `global = true` in
// `cli.rs`, which made clap attach them to every subcommand, including ones
// (`doctor`, `install`, `capabilities`, `update`) that can never read them.
// `--json`, `--quiet`, `--yes`/`--no-install`, and the per-backend
// `--<x>-path` overrides genuinely do mean something on every subcommand
// (several of them can install a missing backend), so those stay global.

/// None of the four conversion-only flags should appear in `--help` for any
/// subcommand that can never read them.
#[test]
fn subcommand_help_never_lists_conversion_only_flags() {
    for subcommand in ["doctor", "install", "capabilities", "update"] {
        let out = conv().args([subcommand, "--help"]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        for flag in ["--dry-run", "--overwrite", "--outdir", "--jobs"] {
            assert!(
                !stdout.contains(flag),
                "`conv {subcommand} --help` must not list {flag}: {stdout}"
            );
        }
        // Short forms too -- `-y`, `-o`, `-j` are single-letter and could
        // otherwise false-negative past a substring check on the long form.
        for short in ["-y,", "-o,", "-j,"] {
            assert!(
                !stdout.contains(short),
                "`conv {subcommand} --help` must not list the {short} short flag: {stdout}"
            );
        }
    }
}

/// The flags that genuinely apply everywhere (several subcommands can
/// install a missing backend) must still be listed on every subcommand.
#[test]
fn subcommand_help_still_lists_the_genuinely_global_flags() {
    for subcommand in ["doctor", "install", "capabilities", "update"] {
        let out = conv().args([subcommand, "--help"]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        for flag in [
            "--json",
            "--quiet",
            "--yes",
            "--no-install",
            "--ffmpeg-path",
            "--ffprobe-path",
            "--magick-path",
            "--pandoc-path",
            "--soffice-path",
            "--typst-path",
        ] {
            assert!(
                stdout.contains(flag),
                "`conv {subcommand} --help` must still list {flag}: {stdout}"
            );
        }
    }
}

/// Not just cosmetic: a conversion-only flag placed *after* a subcommand
/// name must be rejected as a usage error, not silently accepted.
#[test]
fn a_conversion_only_flag_after_a_subcommand_is_rejected() {
    conv()
        .args(["doctor", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("unexpected argument"));
    conv()
        .args(["update", "-o", "somewhere"])
        .assert()
        .code(2)
        .stderr(contains("unexpected argument"));
}

/// The conversion path itself must still accept all four flags together --
/// removing `global = true` must not have accidentally removed the fields.
#[test]
fn the_conversion_path_still_accepts_every_conversion_only_flag() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jpg"), b"not a real jpg").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["a.jpg", ".png", "--dry-run", "-y", "-o", "out", "-j", "2"])
        .assert()
        .success()
        .stdout(contains("magick"));
}
