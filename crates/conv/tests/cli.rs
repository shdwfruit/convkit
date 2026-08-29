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
