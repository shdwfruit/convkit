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
