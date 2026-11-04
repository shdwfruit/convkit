//! JSON, YAML, TOML and CSV: the conversions conv runs itself, and
//! `conv fmt`.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn conv(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("conv").unwrap();
    cmd.current_dir(dir);
    cmd
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

const PACKAGE: &str = r#"{
  "name": "demo",
  "version": "1.0.0",
  "private": true,
  "scripts": {"build": "tsc", "test": "jest --ci"},
  "dependencies": {"@scope/pkg": "^2.1.0", "react": "^19.0.0"},
  "big": 12345678901234567890123,
  "ratio": 0.1,
  "none": null
}"#;

#[test]
fn json_converts_to_yaml_and_back_exactly() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", PACKAGE);
    conv(dir.path())
        .args(["package.json", ".yaml"])
        .assert()
        .success()
        .stdout(contains("OK package.yaml"));
    let yaml = read(dir.path(), "package.yaml");
    assert!(yaml.contains("'@scope/pkg': ^2.1.0"), "{yaml}");
    assert!(yaml.contains("big: 12345678901234567890123"), "{yaml}");
    assert!(yaml.contains("ratio: 0.1"), "{yaml}");
    conv(dir.path())
        .args(["package.yaml", "back.json"])
        .assert()
        .success();
    let pretty = conv(dir.path())
        .args(["fmt", "package.json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(read(dir.path(), "back.json").as_bytes(), pretty.as_slice());
}

#[test]
fn capabilities_lists_the_data_pairs_as_built_in() {
    let dir = tempfile::tempdir().unwrap();
    let out = conv(dir.path())
        .args(["capabilities", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let built: Vec<(String, String)> = v["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["built_in"] == true)
        .map(|p| {
            assert_eq!(p["backends"], serde_json::json!([]));
            (
                p["from"].as_str().unwrap().to_string(),
                p["to"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for (from, to) in [
        ("json", "yaml"),
        ("json", "toml"),
        ("json", "csv"),
        ("yaml", "json"),
        ("yaml", "toml"),
        ("yaml", "csv"),
        ("toml", "json"),
        ("toml", "yaml"),
        ("csv", "json"),
        ("csv", "yaml"),
    ] {
        assert!(
            built.contains(&(from.to_string(), to.to_string())),
            "{from} -> {to} in {built:?}"
        );
    }
    // Archives are built in too; of the data formats, these are all.
    let data = ["json", "yaml", "toml", "csv"];
    let ours = built
        .iter()
        .filter(|(f, _)| data.contains(&f.as_str()))
        .count();
    assert_eq!(ours, 10, "{built:?}");
    conv(dir.path())
        .args(["capabilities", "json"])
        .assert()
        .success()
        .stdout(contains("json -> yaml     built in"))
        .stdout(contains("conv fmt formats and checks it"));
}

#[test]
fn a_dry_run_says_it_is_built_in() {
    let dir = tempfile::tempdir().unwrap();
    conv(dir.path())
        .args(["in.json", "out.toml", "--dry-run"])
        .assert()
        .success()
        .stdout("built in: in.json to out.toml, no backend\n");
    let out = conv(dir.path())
        .args(["in.json", "out.toml", "--dry-run", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let plan = &v["plans"][0]["plan"];
    assert_eq!(plan["built_in"], true);
    assert_eq!(plan["steps"], serde_json::json!([]));
}

#[test]
fn an_invalid_file_fails_with_the_spot_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "bad.json", "{\n  \"a\": 1\n  \"b\": 2\n}\n");
    conv(dir.path())
        .args(["bad.json", ".yaml"])
        .assert()
        .code(1)
        .stderr(contains(
            "bad.json: line 3, column 3: a comma is missing before this",
        ))
        .stderr(contains("   3 |   \"b\": 2"))
        .stderr(contains("     |   ^"));
    assert!(!dir.path().join("bad.yaml").exists());
    let out = conv(dir.path())
        .args(["bad.json", ".yaml", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["results"][0]["error"]["code"], "invalid_data");
}

#[test]
fn what_the_target_cannot_hold_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.json", r#"{"server": {"port": null}}"#);
    conv(dir.path())
        .args(["a.json", ".toml"])
        .assert()
        .code(1)
        .stderr(contains("TOML has no null, and server.port is null"));
    write(dir.path(), "rows.csv", "a,b\n1,2\n");
    conv(dir.path())
        .args(["rows.csv", ".toml"])
        .assert()
        .code(2)
        .stderr(contains("TOML has no top-level list"));
    conv(dir.path())
        .args(["a.json", ".json"])
        .assert()
        .code(2)
        .stderr(contains("use conv fmt"));
    conv(dir.path())
        .args(["a.json", "out.yaml", "--resize", "50%"])
        .assert()
        .code(2)
        .stderr(contains("--resize does not apply to json -> yaml"));
}

#[test]
fn a_csv_becomes_typed_rows_and_a_folder_converts_as_a_batch() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "people.csv",
        "name,zip,age\nAda,02134,36\nBo,10001,\n",
    );
    conv(dir.path())
        .args(["people.csv", ".json"])
        .assert()
        .success()
        .stdout(contains("zip codes and IDs, are kept as text"));
    assert_eq!(
        read(dir.path(), "people.json"),
        "[\n  {\n    \"name\": \"Ada\",\n    \"zip\": \"02134\",\n    \"age\": 36\n  },\n  {\n    \"name\": \"Bo\",\n    \"zip\": \"10001\",\n    \"age\": null\n  }\n]\n"
    );
    let sub = dir.path().join("configs");
    std::fs::create_dir(&sub).unwrap();
    write(&sub, "a.json", "{\"x\": 1}");
    write(&sub, "b.toml", "y = 2\n");
    conv(dir.path())
        .args(["configs", "--to", "yaml"])
        .assert()
        .success()
        .stdout(contains("2 converted"));
    assert_eq!(read(&sub, "a.yaml"), "x: 1\n");
    assert_eq!(read(&sub, "b.yaml"), "y: 2\n");
}

#[test]
fn fmt_pretty_prints_packs_and_sorts() {
    let dir = tempfile::tempdir().unwrap();
    let input = r#"{"b":[1,2.50],"a":{"c":true}}"#;
    conv(dir.path())
        .arg("fmt")
        .write_stdin(input)
        .assert()
        .success()
        .stdout("{\n  \"b\": [\n    1,\n    2.50\n  ],\n  \"a\": {\n    \"c\": true\n  }\n}\n");
    conv(dir.path())
        .args(["fmt", "--minify", "--sort-keys"])
        .write_stdin(input)
        .assert()
        .success()
        .stdout("{\"a\":{\"c\":true},\"b\":[1,2.50]}\n");
    conv(dir.path())
        .args(["fmt", "--indent", "4", "--to", "yaml"])
        .write_stdin(input)
        .assert()
        .success()
        .stdout("b:\n    - 1\n    - 2.50\na:\n    c: true\n");
    conv(dir.path())
        .args(["fmt", "--tab", "-"])
        .write_stdin("[1]")
        .assert()
        .success()
        .stdout("[\n\t1\n]\n");
    // Piped output is plain, with no colour codes.
    let out = conv(dir.path())
        .args(["fmt", "--to", "toml"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "b = [1, 2.50]\n\n[a]\nc = true\n"
    );
}

#[test]
fn fmt_points_at_the_mistake() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "bad.json", "{\n  \"tags\": [\"a\",],\n}\n");
    conv(dir.path())
        .args(["fmt", "bad.json"])
        .assert()
        .code(1)
        .stdout("")
        .stderr(contains("FAIL bad.json is not valid JSON"))
        .stderr(contains("line 2, column 15: a comma after the last item"))
        .stderr(contains(
            "   2 |   \"tags\": [\"a\",],\n     |               ^",
        ))
        .stderr(contains("--lenient"));
    let out = conv(dir.path())
        .args(["fmt", "bad.json", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let e = &v["results"][0]["error"];
    assert_eq!(
        (e["line"].as_u64(), e["column"].as_u64()),
        (Some(2), Some(15))
    );
    assert_eq!(e["code"], "invalid_data");
    conv(dir.path())
        .args(["fmt", "--lenient", "bad.json"])
        .assert()
        .success()
        .stdout("{\n  \"tags\": [\n    \"a\"\n  ]\n}\n")
        .stderr(contains("JSON5, not JSON: it has trailing commas"));
}

#[test]
fn fmt_checks_without_printing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "ci.yml", "on:\n  push:\n    branches: [main]\n");
    conv(dir.path())
        .args(["fmt", "--check", "ci.yml"])
        .assert()
        .success()
        .stdout("OK ci.yml is valid YAML\n");
    write(dir.path(), "dup.yml", "a: 1\na: 2\n");
    conv(dir.path())
        .args(["fmt", "--check", "dup.yml"])
        .assert()
        .code(1)
        .stderr(contains(
            "line 2, column 1: the key \"a\" is in this mapping twice",
        ));
}

#[test]
fn fmt_writes_over_its_input_only_with_y_and_never_drops_its_comments() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.json", "{\"a\":1}");
    conv(dir.path())
        .args(["fmt", "data.json", "data.json"])
        .assert()
        .code(2)
        .stderr(contains("exists; pass -y"));
    conv(dir.path())
        .args(["fmt", "data.json", ".json", "-y"])
        .assert()
        .success();
    assert_eq!(read(dir.path(), "data.json"), "{\n  \"a\": 1\n}\n");

    let commented = "# keep me\nname: x\n";
    write(dir.path(), "conf.yaml", commented);
    conv(dir.path())
        .args(["fmt", "conf.yaml", "conf.yaml", "-y"])
        .assert()
        .code(2)
        .stderr(contains(
            "conf.yaml has a comment, which conv's output can't keep",
        ));
    assert_eq!(read(dir.path(), "conf.yaml"), commented);
    // Printed, or written elsewhere, it is said and done.
    conv(dir.path())
        .args(["fmt", "conf.yaml"])
        .assert()
        .success()
        .stdout("name: x\n")
        .stderr(contains("The comment in conf.yaml is not kept."));
}

#[test]
fn fmt_writes_into_a_folder_and_converts_by_extension() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    );
    conv(dir.path())
        .args(["fmt", "Cargo.toml", "-o", "out", "--to", "json"])
        .assert()
        .success();
    assert_eq!(
        read(&dir.path().join("out"), "Cargo.json"),
        "{\n  \"package\": {\n    \"name\": \"x\",\n    \"version\": \"0.1.0\"\n  }\n}\n"
    );
    conv(dir.path())
        .args(["fmt", "Cargo.toml", ".yaml"])
        .assert()
        .success();
    assert_eq!(
        read(dir.path(), "Cargo.yaml"),
        "package:\n  name: x\n  version: 0.1.0\n"
    );
    conv(dir.path())
        .args(["fmt", "Cargo.toml", "x.yaml", "--to", "json"])
        .assert()
        .code(2)
        .stderr(contains("name two languages"));
    conv(dir.path())
        .args(["fmt", "--tab", "--to", "yaml", "Cargo.toml"])
        .assert()
        .code(2)
        .stderr(contains("YAML can't indent with tabs"));
}

#[test]
fn fmt_tells_the_language_of_piped_text() {
    let dir = tempfile::tempdir().unwrap();
    conv(dir.path())
        .args(["fmt", "--to", "json"])
        .write_stdin("[server]\nport = 8080\n")
        .assert()
        .success()
        .stdout("{\n  \"server\": {\n    \"port\": 8080\n  }\n}\n");
    conv(dir.path())
        .args(["fmt", "--to", "json"])
        .write_stdin("- a\n- b\n")
        .assert()
        .success()
        .stdout("[\n  \"a\",\n  \"b\"\n]\n");
    conv(dir.path())
        .args(["fmt", "--from", "csv", "--to", "json"])
        .write_stdin("a,b\n1,x\n")
        .assert()
        .success()
        .stdout("[\n  {\n    \"a\": 1,\n    \"b\": \"x\"\n  }\n]\n");
    conv(dir.path())
        .args(["fmt"])
        .write_stdin("key: [1, 2\n")
        .assert()
        .code(1)
        .stderr(contains("standard input is not valid YAML"))
        .stderr(contains("--from says what it is"));
}

#[test]
fn fmt_streams_a_large_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let rows: Vec<String> = (0..40_000)
        .map(|i| format!("{{\"id\":{i},\"name\":\"row {i}\",\"tags\":[\"a\",\"b\"]}}"))
        .collect();
    let input = format!("[{}]", rows.join(","));
    let out = conv(dir.path())
        .args(["fmt", "--to", "yaml"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("- id: 0\n  name: row 0\n  tags:\n    - a\n    - b\n"));
    assert!(text.ends_with("- id: 39999\n  name: row 39999\n  tags:\n    - a\n    - b\n"));
}

#[test]
fn fmt_refuses_what_it_does_not_read() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "photo.png", "not a png");
    conv(dir.path())
        .args(["fmt", "photo.png"])
        .assert()
        .code(2)
        .stderr(contains("conv fmt reads JSON, YAML, TOML and CSV"));
    conv(dir.path())
        .args(["fmt", "missing.json"])
        .assert()
        .code(2)
        .stderr(contains("input not found"));
    write(dir.path(), "rows.csv", "a\n1\n");
    conv(dir.path())
        .args(["fmt", "rows.csv"])
        .assert()
        .code(2)
        .stderr(contains("doesn't reformat a CSV").and(contains("--to json")));
}
