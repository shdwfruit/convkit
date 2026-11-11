//! The text and developer tools, run as `conv` runs them.

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;

fn conv() -> Command {
    let mut cmd = Command::cargo_bin("conv").unwrap();
    // A fixed zone, so local times read the same on every machine.
    cmd.env("TZ", "America/New_York");
    cmd
}

fn json(args: &[&str]) -> Value {
    let out = conv().args(args).arg("--json").output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn stdout(cmd: &mut Command) -> String {
    String::from_utf8(cmd.output().unwrap().stdout).unwrap()
}

/// jwt.io's example token, signed with "your-256-bit-secret".
const JWT_IO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

/// HS256 over {"alg":"HS256"} and {"exp":1000} with the secret "k":
/// expired in 1970.
const EXPIRED: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjEwMDB9.I4YZdwa500yZeTUnA5IPZ7DQJIqgEk5z9CpvKwh-Now";

#[test]
fn help_lists_the_tools_after_the_file_commands() {
    let help = stdout(conv().arg("--help"));
    let split = help.find("  split ").unwrap();
    let mut last = split;
    for tool in [
        "base64", "hash", "uuid", "jwt", "time", "link", "count", "case", "chars", "lorem", "diff",
    ] {
        let at = help
            .find(&format!("  {tool} "))
            .unwrap_or_else(|| panic!("{tool} missing: {help}"));
        assert!(at > last, "{tool} out of order");
        last = at;
    }
}

// --- base64 -------------------------------------------------------------

#[test]
fn base64_encodes_and_decodes() {
    conv()
        .args(["base64", "hello"])
        .assert()
        .success()
        .stdout("aGVsbG8=\n");
    conv()
        .args(["base64", "-d", "aGVsbG8"])
        .assert()
        .success()
        .stdout("hello\n");
    conv()
        .args(["base64"])
        .write_stdin("hi there")
        .assert()
        .success()
        .stdout("aGkgdGhlcmU=\n");
    conv()
        .args(["base64", "--url", "-"])
        .write_stdin(vec![0xfbu8, 0xff])
        .assert()
        .success()
        .stdout("-_8\n");
}

#[test]
fn base64_refuses_what_isnt_base64_and_says_where() {
    conv()
        .args(["base64", "-d", "aGV!sbG8="])
        .assert()
        .code(1)
        .stderr(contains("'!' at character 4"));
    conv()
        .args(["base64", "-d", "aGVsb"])
        .assert()
        .code(1)
        .stderr(contains("a character is missing or one too many"));
    conv()
        .args(["base64", "-d", "ab+-"])
        .assert()
        .code(1)
        .stderr(contains("mixes the standard"));
}

#[test]
fn base64_makes_and_reads_data_uris() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("dot.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
    let uri =
        stdout(
            conv()
                .current_dir(dir.path())
                .args(["base64", "-f", "dot.png", "--data-uri"]),
        );
    assert_eq!(uri, "data:image/png;base64,iVBORw0KGgpyZXN0\n");
    // Back into a file named for its type.
    conv()
        .current_dir(dir.path())
        .args(["base64", "-d", uri.trim(), "-o", "out"])
        .assert()
        .success()
        .stdout(contains("decoded.png"));
    let written = std::fs::read(dir.path().join("out/decoded.png")).unwrap();
    assert_eq!(written, b"\x89PNG\r\n\x1a\nrest");
    // Not over a file that is there.
    conv()
        .current_dir(dir.path())
        .args(["base64", "-d", uri.trim(), "-o", "out"])
        .assert()
        .code(2)
        .stderr(contains("pass -y to overwrite"));
}

#[test]
fn base64_sends_binary_down_a_pipe_as_bytes() {
    let out = conv()
        .args(["base64", "-d", "iVBORw0KGgpyZXN0"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"\x89PNG\r\n\x1a\nrest");
}

#[test]
fn text_that_names_a_file_is_still_text_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "the file").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["base64", "notes.txt"])
        .assert()
        .success()
        .stdout("bm90ZXMudHh0\n")
        .stderr(contains("use -f notes.txt"));
    conv()
        .current_dir(dir.path())
        .args(["base64", "-f", "notes.txt"])
        .assert()
        .success()
        .stdout("dGhlIGZpbGU=\n");
}

#[test]
fn input_comes_one_way_only() {
    conv()
        .args(["count", "words", "-f", "x.txt"])
        .assert()
        .code(2)
        .stderr(contains("its input one way"));
}

#[test]
fn base64_json() {
    let v = json(&["base64", "hello", "--url"]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["results"][0]["encoded"], "aGVsbG8");
    assert_eq!(v["results"][0]["alphabet"], "url-safe");
}

// --- hash ---------------------------------------------------------------

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

#[test]
fn hash_prints_sha256_as_sha256sum_does() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.txt"), "hello").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["hash", "h.txt"])
        .assert()
        .success()
        .stdout(format!("{HELLO_SHA256}  h.txt\n"));
    conv()
        .args(["hash"])
        .write_stdin("hello")
        .assert()
        .success()
        .stdout(format!("{HELLO_SHA256}  -\n"));
    conv()
        .args(["hash", "--text", "hello", "--md5"])
        .assert()
        .success()
        .stdout("5d41402abc4b2a76b9719d911017c592\n");
}

#[test]
fn hash_all_lists_five() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.txt"), "hello").unwrap();
    let out = stdout(
        conv()
            .current_dir(dir.path())
            .args(["hash", "--all", "h.txt"]),
    );
    for name in ["MD5", "SHA-1", "SHA-256", "SHA-384", "SHA-512"] {
        assert!(out.contains(name), "{out}");
    }
    let v = json(&["hash", "--all", "--text", "hello"]);
    assert_eq!(
        v["files"][0]["sha1"],
        "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
    );
}

#[test]
fn hash_check_matches_in_any_case_and_fails_plainly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.txt"), "hello").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["hash", "h.txt", "--check", &HELLO_SHA256.to_uppercase()])
        .assert()
        .success()
        .stdout(contains("h.txt matches (SHA-256)"));
    // MD5 by its length, with a word about what MD5 is worth.
    conv()
        .current_dir(dir.path())
        .args([
            "hash",
            "h.txt",
            "--check",
            "5d41402abc4b2a76b9719d911017c592",
        ])
        .assert()
        .success()
        .stdout(contains("matches (MD5)"))
        .stderr(contains("a SHA-256 says more"));
    conv()
        .current_dir(dir.path())
        .args(["hash", "h.txt", "--check", &"0".repeat(64)])
        .assert()
        .code(1)
        .stderr(contains("h.txt does NOT match"))
        .stderr(contains(format!("actual    {HELLO_SHA256}")));
    conv()
        .current_dir(dir.path())
        .args(["hash", "h.txt", "--check", "abc123"])
        .assert()
        .code(2)
        .stderr(contains("6 hex digits"));
    let v = json(&["hash", "--text", "hello", "--check", &"0".repeat(64)]);
    assert_eq!(v["matched"], false);
    assert_eq!(v["files"][0]["check"]["match"], false);
}

#[test]
fn hash_check_with_a_file_points_at_sums() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("SHA256SUMS"), "x").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["hash", "h.txt", "--check", "SHA256SUMS"])
        .assert()
        .code(2)
        .stderr(contains("use --sums SHA256SUMS"));
}

#[test]
fn hash_sums_checks_what_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("h.txt"), "hello").unwrap();
    std::fs::write(d.join("e.txt"), "").unwrap();
    std::fs::write(
        d.join("SHA256SUMS"),
        format!(
            "{HELLO_SHA256}  h.txt\nSHA256 (e.txt) = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n{}  gone.iso\n",
            "1".repeat(64)
        ),
    )
    .unwrap();
    // From elsewhere: the files are found next to the sums file.
    let sums = d.join("SHA256SUMS");
    conv()
        .args(["hash", "--sums"])
        .arg(&sums)
        .assert()
        .success()
        .stdout(contains("2 files match SHA256SUMS"))
        .stderr(contains("1 file that SHA256SUMS lists isn't here"));
    std::fs::write(d.join("h.txt"), "changed").unwrap();
    conv()
        .args(["hash", "--sums"])
        .arg(&sums)
        .assert()
        .code(1)
        .stderr(contains("h.txt does NOT match"))
        .stdout(contains("1 of 2 files doesn't match SHA256SUMS"));
    // One file, by name.
    conv()
        .current_dir(d)
        .args(["hash", "--sums", "SHA256SUMS", "e.txt"])
        .assert()
        .success()
        .stdout(contains("1 file matches"));
}

#[test]
fn hash_sums_prefers_the_exact_path_over_the_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (sub, body) in [("dir1", "hello"), ("dir2", "")] {
        std::fs::create_dir(d.join(sub)).unwrap();
        std::fs::write(d.join(sub).join("app.zip"), body).unwrap();
    }
    std::fs::write(
        d.join("SHA256SUMS"),
        format!(
            "{HELLO_SHA256}  dir1/app.zip\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  dir2/app.zip\n"
        ),
    )
    .unwrap();
    conv()
        .current_dir(d)
        .args(["hash", "--sums", "SHA256SUMS", "dir2/app.zip"])
        .assert()
        .success();
}

#[test]
fn hash_missing_files_and_folders() {
    conv()
        .args(["hash", "no-such-file.bin"])
        .assert()
        .code(2)
        .stderr(contains("to hash the text itself, use --text"));
    let dir = tempfile::tempdir().unwrap();
    conv()
        .arg("hash")
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(contains("is a folder"));
}

// --- uuid ---------------------------------------------------------------

fn is_uuid(s: &str, version: char) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
        && parts[2].starts_with(version)
}

#[test]
fn uuid_makes_v4_and_sorted_v7() {
    let one = stdout(conv().arg("uuid"));
    assert!(is_uuid(one.trim(), '4'), "{one}");
    let many = stdout(conv().args(["uuid", "--v7", "-n", "5"]));
    let ids: Vec<&str> = many.lines().collect();
    assert_eq!(ids.len(), 5);
    assert!(ids.iter().all(|id| is_uuid(id, '7')));
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
}

#[test]
fn uuid_reads_one() {
    conv()
        .args(["uuid", "{017F22E2-79B0-7CC3-98C4-DC0C0C07398F}"])
        .assert()
        .success()
        .stdout(contains("017f22e2-79b0-7cc3-98c4-dc0c0c07398f"))
        .stdout(contains("version  7"))
        .stdout(contains("Tue 2022-02-22 14:22:22 EST"));
    let v = json(&["uuid", "C232AB00-9414-11EC-B3C8-9F6BDECED846"]);
    assert_eq!(v["uuids"][0]["version"], 1);
    assert_eq!(v["uuids"][0]["timestamp"], "2022-02-22T19:22:22Z");
    assert_eq!(v["uuids"][0]["node"], "9f:6b:de:ce:d8:46");
    conv()
        .args(["uuid", "not-a-uuid"])
        .assert()
        .code(1)
        .stderr(contains("isn't a UUID"));
    conv()
        .args(["uuid", "-"])
        .write_stdin("017f22e2-79b0-7cc3-98c4-dc0c0c07398f\n")
        .assert()
        .success()
        .stdout(contains("version  7"));
}

// --- jwt ----------------------------------------------------------------

#[test]
fn jwt_decodes_and_never_needs_a_secret() {
    conv()
        .args(["jwt", JWT_IO])
        .assert()
        .success()
        .stdout(contains("\"name\": \"John Doe\""))
        .stdout(contains("issued     Wed 2018-01-17 20:30:22 EST"))
        .stdout(contains("expires    never"))
        .stdout(contains("not checked; --verify checks it"));
    conv()
        .args(["jwt"])
        .write_stdin(format!("Bearer {JWT_IO}\n"))
        .assert()
        .success();
}

#[test]
fn jwt_checks_hmac_with_a_secret_from_the_environment() {
    conv()
        .args(["jwt", JWT_IO, "--secret-env", "JWT_SECRET"])
        .env("JWT_SECRET", "your-256-bit-secret")
        .assert()
        .success()
        .stdout(contains("valid (HS256), checked with the secret"));
    conv()
        .args(["jwt", JWT_IO, "--secret-env", "JWT_SECRET"])
        .env("JWT_SECRET", "not it")
        .assert()
        .code(1)
        .stdout(contains("INVALID (HS256)"));
    conv()
        .args(["jwt", JWT_IO, "--secret-env", "UNSET_FOR_SURE_1234"])
        .env_remove("UNSET_FOR_SURE_1234")
        .assert()
        .code(2)
        .stderr(contains("isn't set"));
}

#[test]
fn jwt_secret_on_the_command_line_works_with_a_warning() {
    conv()
        .args(["jwt", JWT_IO, "--secret", "your-256-bit-secret"])
        .assert()
        .success()
        .stderr(contains("stays in your shell's history"));
}

#[test]
fn jwt_verify_needs_a_terminal() {
    conv()
        .args(["jwt", JWT_IO, "--verify"])
        .assert()
        .code(2)
        .stderr(contains("--secret-env"));
}

#[test]
fn jwt_expired_says_so_and_fails_a_check() {
    conv()
        .args(["jwt", EXPIRED])
        .assert()
        .success()
        .stdout(contains("expired    Wed 1969-12-31 19:16:40 EST"))
        .stdout(contains("status     expired"));
    // The signature is right, but the token isn't good now.
    conv()
        .args(["jwt", EXPIRED, "--secret-env", "K"])
        .env("K", "k")
        .assert()
        .code(1)
        .stdout(contains("valid (HS256)"));
    let v = json(&["jwt", EXPIRED]);
    assert_eq!(v["tokens"][0]["expired"], true);
    assert_eq!(v["tokens"][0]["times"]["exp"]["unix"], 1000);
}

#[test]
fn jwt_malformed_tokens() {
    for (token, says) in [
        ("abc.def", "this has 2"),
        ("a.b.c.d.e", "encrypted token (JWE"),
        ("eyJhbGc!.e30.", "header (part 1) isn't base64url"),
        ("aGVsbG8.e30.", "header (part 1) isn't JSON"),
        ("e30.aGVsbG8.", "claims (part 2) aren't JSON"),
    ] {
        conv()
            .args(["jwt", token])
            .assert()
            .code(1)
            .stderr(contains(says));
    }
}

// --- time ---------------------------------------------------------------

#[test]
fn time_reads_a_timestamp_and_says_its_unit() {
    conv()
        .args(["time", "1730642400000"])
        .assert()
        .success()
        .stdout(contains("read as   milliseconds (13 digits"))
        .stdout(contains("unix      1730642400\n"))
        .stdout(contains("utc       Sun 2024-11-03 14:00:00 UTC"))
        .stdout(contains(
            "local     Sun 2024-11-03 09:00:00 EST (-05:00, America/New_York)",
        ));
    conv()
        .args(["time", "1730642400", "--unit", "ms"])
        .assert()
        .success()
        .stdout(contains("1970-01-21"));
    conv()
        .args(["time", "-1"])
        .assert()
        .success()
        .stdout(contains("1969-12-31 23:59:59 UTC"));
}

#[test]
fn time_reads_dates_and_other_zones() {
    let v = json(&[
        "time",
        "Tue, 1 Jul 2003 10:52:37 +0200",
        "--tz",
        "Asia/Tokyo",
    ]);
    assert_eq!(v["times"][0]["unix"], 1057049557);
    assert_eq!(
        v["times"][0]["zones"][0]["time"],
        "2003-07-01T17:52:37+09:00"
    );
    conv()
        .args(["time", "2024-02-29"])
        .assert()
        .success()
        .stdout(contains("unix      1709182800"))
        .stderr(contains(
            "no zone given, so read in yours, America/New_York",
        ));
    conv()
        .args(["time", "2024-01-01", "--tz", "Mars/Olympus"])
        .assert()
        .code(2)
        .stderr(contains("doesn't know the time zone"));
}

#[test]
fn time_dst_leap_years_and_ambiguity() {
    conv()
        .args(["time", "2024-03-10 02:30"])
        .assert()
        .success()
        .stdout(contains("Sun 2024-03-10 03:30:00 EDT"))
        .stderr(contains("never happened in America/New_York"));
    conv()
        .args(["time", "2024-11-03 01:30"])
        .assert()
        .success()
        .stderr(contains("happened twice"));
    conv()
        .args(["time", "2023-02-29"])
        .assert()
        .code(1)
        .stderr(contains("2023 isn't a leap year"));
    conv()
        .args(["time", "03/04/2024"])
        .assert()
        .code(1)
        .stderr(contains("4 March or 3 April"));
}

#[test]
fn time_now_and_relative() {
    let v = json(&["time"]);
    assert_eq!(v["times"][0]["read_as"], "now");
    assert_eq!(v["times"][0]["relative"], "now");
    let v = json(&["time", "3 days ago"]);
    assert_eq!(v["times"][0]["relative"], "3 days ago");
    // No pipe is waited on without -.
    conv().arg("time").write_stdin("1").assert().success();
    conv()
        .args(["time", "-"])
        .write_stdin("0\n")
        .assert()
        .success()
        .stdout(contains("1970-01-01 00:00:00 UTC"));
}

// --- link ---------------------------------------------------------------

#[test]
fn link_takes_tracking_out() {
    conv()
        .args([
            "link",
            "https://example.com/a?utm_source=x&id=7&fbclid=abc#top",
            "https://youtu.be/dQw4w9WgXcQ?si=abc&t=42",
        ])
        .assert()
        .success()
        .stdout("https://example.com/a?id=7#top\nhttps://youtu.be/dQw4w9WgXcQ?t=42\n")
        .stderr(contains("removed utm_source, fbclid"));
}

#[test]
fn link_unwraps_redirects() {
    let v = json(&[
        "link",
        "https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fexample.com%2Fdoc%3Futm_medium%3Demail&data=05",
    ]);
    assert_eq!(v["links"][0]["output"], "https://example.com/doc");
    assert_eq!(v["links"][0]["unwrapped"][0], "Outlook Safe Links");
}

#[test]
fn link_cleans_every_link_in_a_text() {
    conv()
        .arg("link")
        .write_stdin(
            "Read https://example.com/p?utm_campaign=z, then (https://x.com/a/status/1?s=20).\n",
        )
        .assert()
        .success()
        .stdout("Read https://example.com/p, then (https://x.com/a/status/1).\n");
}

#[test]
fn link_names_short_links_and_never_asks_them_unasked() {
    conv()
        .args(["link", "https://bit.ly/3abc"])
        .assert()
        .success()
        .stdout("https://bit.ly/3abc\n")
        .stderr(contains("--follow asks bit.ly"));
    conv()
        .args(["link", "example.com/x"])
        .assert()
        .code(1)
        .stderr(contains("write it as https://example.com/x"));
}

#[test]
fn link_follow_in_a_text_asks_only_short_links() {
    // A request to this closed port would fail the command.
    conv()
        .args(["link", "--follow"])
        .write_stdin("http://127.0.0.1:1/magic-login?token=abc\n")
        .assert()
        .success();
}

// --- count, case, chars, lorem -------------------------------------------

#[test]
fn count_words_and_characters() {
    let v = json(&["count", "The quick brown fox. It jumped!\n\nAgain."]);
    let c = &v["counts"][0];
    assert_eq!(c["words"], 7);
    assert_eq!(c["sentences"], 3);
    assert_eq!(c["paragraphs"], 2);
    // CJK characters a word each; an emoji family one character.
    let v = json(&["count", "我喜欢猫 👨‍👩‍👧"]);
    assert_eq!(v["counts"][0]["words"], 4);
    assert_eq!(v["counts"][0]["characters"], 6);
}

#[test]
fn count_reads_a_crlf_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("w.txt"), "one two\r\nthree\r\n").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["count", "-f", "w.txt"])
        .assert()
        .success()
        .stdout(contains("words                  3"))
        .stdout(contains("lines                  2"));
}

#[test]
fn case_changes_case() {
    for (style, out) in [
        ("snake", "hello_world_its_xml_http_request"),
        ("camel", "helloWorldItsXmlHttpRequest"),
        ("constant", "HELLO_WORLD_ITS_XML_HTTP_REQUEST"),
        ("kebab", "hello-world-its-xml-http-request"),
    ] {
        conv()
            .args(["case", style, "Hello world, it's XMLHttpRequest"])
            .assert()
            .success()
            .stdout(format!("{out}\n"));
    }
    conv()
        .args(["case", "upper", "straße"])
        .assert()
        .success()
        .stdout("STRASSE\n");
    conv()
        .args(["case", "snake"])
        .write_stdin("First Name\nLast Name\n")
        .assert()
        .success()
        .stdout("first_name\nlast_name\n");
    conv().args(["case", "wavy", "x"]).assert().code(2);
}

#[test]
fn chars_flags_what_hides() {
    conv()
        .args(["chars", "p\u{430}y\u{200b}ok"])
        .assert()
        .success()
        .stdout(contains("CYRILLIC SMALL LETTER A"))
        .stdout(contains("looks like Latin a"))
        .stdout(contains("[ZWSP]"))
        .stdout(contains(
            "suspicious: 1 invisible character (at 1:4), 1 lookalike (at 1:2)",
        ));
    conv()
        .args(["chars", "👨‍👩‍👧 ok"])
        .assert()
        .success()
        .stdout(contains("5 code points"))
        .stdout(contains("nothing suspicious in 8 characters"));
}

#[test]
fn chars_suspicious_gives_places_and_never_prints_bidi_raw() {
    let out = conv()
        .args(["chars", "--suspicious", "-"])
        .write_stdin("ok\nif (a\u{202e}) {}\n")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("2:6  [RLO]"), "{text}");
    assert!(!text.contains('\u{202e}'), "{text}");
    let v = json(&["chars", "a\u{a0}b"]);
    assert_eq!(v["chars"][1]["flag"]["kind"], "space");
    assert_eq!(v["suspicious"][0]["count"], 1);
}

#[test]
fn lorem_is_repeatable_with_a_seed() {
    let a = stdout(conv().args(["lorem", "-p", "3", "--seed", "7"]));
    let b = stdout(conv().args(["lorem", "-p", "3", "--seed", "7"]));
    assert_eq!(a, b);
    assert!(a.starts_with("Lorem ipsum dolor sit amet"));
    assert_eq!(a.trim_end().split("\n\n").count(), 3);
    conv()
        .args(["lorem", "-w", "5"])
        .assert()
        .success()
        .stdout("Lorem ipsum dolor sit amet.\n");
    let html = stdout(conv().args(["lorem", "-p", "2", "--html"]));
    assert_eq!(html.matches("<p>").count(), 2);
    let random = stdout(conv().args(["lorem", "-s", "2", "--random"]));
    assert!(!random.starts_with("Lorem ipsum"));
}

// --- diff ---------------------------------------------------------------

fn two_files(a: &str, b: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), a).unwrap();
    std::fs::write(dir.path().join("b.txt"), b).unwrap();
    dir
}

#[test]
fn diff_marks_changed_words_and_exits_like_diff() {
    let dir = two_files(
        "one\nthe quick brown fox\n",
        "one\nthe quick red fox\nthree\n",
    );
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "b.txt"])
        .assert()
        .code(1)
        .stdout(
            "--- a.txt\n+++ b.txt\n@@ -1,2 +1,3 @@\n one\n-the quick [-brown-] fox\n+the quick {+red+} fox\n+three\n",
        );
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "a.txt"])
        .assert()
        .code(0)
        .stdout("");
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "nope.txt"])
        .assert()
        .code(2);
    conv()
        .current_dir(dir.path())
        .args(["diff", "-q", "a.txt", "b.txt"])
        .assert()
        .code(1)
        .stdout("");
}

#[test]
fn diff_unified_is_plain_for_patch() {
    let dir = two_files("a\nb\n", "a\nc\n");
    conv()
        .current_dir(dir.path())
        .args(["diff", "--unified", "a.txt", "b.txt"])
        .assert()
        .code(1)
        .stdout("--- a.txt\n+++ b.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
}

#[test]
fn diff_treats_crlf_as_lf_and_says_so() {
    let dir = two_files("one\r\ntwo\r\n", "one\ntwo\n");
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "b.txt"])
        .assert()
        .code(0)
        .stderr(contains("a.txt ends its lines with CRLF and b.txt with LF"));
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "b.txt", "--eol"])
        .assert()
        .code(1)
        .stdout(contains("-one\u{240d}\n"));
    // A real change in CRLF files.
    let dir = two_files("x\r\ny\r\n", "x\r\nz\r\n");
    conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "b.txt"])
        .assert()
        .code(1)
        .stdout(contains("-y\n+z\n"));
}

#[test]
fn diff_unified_keeps_crs_so_the_patch_applies() {
    let dir = two_files("x\r\ny\r\n", "x\r\nz\r\n");
    conv()
        .current_dir(dir.path())
        .args(["diff", "--unified", "a.txt", "b.txt"])
        .assert()
        .code(1)
        .stdout(contains("-y\r\n+z\r\n"));
    conv()
        .current_dir(dir.path())
        .args(["diff", "-U", "18446744073709551615", "a.txt", "b.txt"])
        .assert()
        .code(2);
}

#[test]
fn diff_texts_stdin_binary_and_json() {
    conv()
        .args(["diff", "--text", "hello world", "hello there"])
        .assert()
        .code(1)
        .stdout("--- a\n+++ b\n@@ -1 +1 @@\n-hello [-world-]\n+hello {+there+}\n");
    let dir = two_files("x\n", "y\n");
    conv()
        .current_dir(dir.path())
        .args(["diff", "-", "b.txt"])
        .write_stdin("y\n")
        .assert()
        .code(0);
    conv()
        .current_dir(dir.path())
        .args(["diff", "-", "-"])
        .assert()
        .code(2);
    std::fs::write(dir.path().join("bin"), b"\x00\x01").unwrap();
    conv()
        .current_dir(dir.path())
        .args(["diff", "bin", "a.txt"])
        .assert()
        .code(1)
        .stdout("Binary files bin and a.txt differ\n");
    let out = conv()
        .current_dir(dir.path())
        .args(["diff", "a.txt", "b.txt", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["same"], false);
    assert_eq!(v["hunks"][0]["lines"][0]["op"], "delete");
    assert_eq!(v["hunks"][0]["lines"][1]["text"], "y");
}

// --- clipboard ------------------------------------------------------------

/// Linux CI has no display, so no clipboard: the copy is reported, and the
/// result is still printed.
#[cfg(target_os = "linux")]
#[test]
fn copy_without_a_clipboard_warns_and_still_prints() {
    use predicates::prelude::PredicateBooleanExt;
    conv()
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(["hash", "--text", "hello", "--copy"])
        .assert()
        .success()
        .stdout(format!("{HELLO_SHA256}\n"))
        .stderr(contains("couldn't copy to the clipboard").and(contains("warning")));
}
