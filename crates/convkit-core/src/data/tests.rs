use super::*;

/// `text` read as `from` and written as `to`, as a conversion reads it
/// (leniently).
fn conv(text: &str, from: Lang, to: Lang) -> Result<(String, Report), DataError> {
    let opts = Options {
        lenient: true,
        ..Options::new("in")
    };
    run_with(text, from, to, &opts)
}

fn run_with(
    text: &str,
    from: Lang,
    to: Lang,
    opts: &Options,
) -> Result<(String, Report), DataError> {
    let mut out = Plain(Vec::new());
    let report = transform(&Source::Text(text.as_bytes()), from, to, opts, &mut out)?;
    Ok((String::from_utf8(out.0).unwrap(), report))
}

fn ok(text: &str, from: Lang, to: Lang) -> String {
    conv(text, from, to)
        .unwrap_or_else(|e| panic!("{from:?} -> {to:?} failed: {e:?}"))
        .0
}

/// `text` checked strictly, as `conv fmt` does: the error.
fn strict_err(text: &str, lang: Lang) -> DataError {
    let opts = Options::new("in");
    check(&Source::Text(text.as_bytes()), lang, &opts).expect_err("should be refused")
}

fn at(e: &DataError) -> (usize, usize) {
    (e.line.unwrap_or(0), e.column.unwrap_or(0))
}

const SAMPLE: &str = r#"{
  "name": "conv",
  "version": "0.6.2",
  "count": 12345678901234567890123,
  "ratio": 0.1,
  "tiny": 1e-400,
  "exact": 1.0,
  "big": 1E+5,
  "neg": -0,
  "ok": true,
  "none": null,
  "nested": {"z": 1, "a": [1, 2.50, "three", {"deep": "é日本🎉"}]},
  "empty": {},
  "list": [],
  "multi": "line one\nline two\n",
  "quote": "it's \"quoted\"\ttab",
  "zip": "02134",
  "looks": "yes"
}"#;

#[test]
fn json_round_trips_through_yaml_exactly() {
    let yaml = ok(SAMPLE, Lang::Json, Lang::Yaml);
    let back = ok(&yaml, Lang::Yaml, Lang::Json);
    let direct = ok(SAMPLE, Lang::Json, Lang::Json);
    assert_eq!(back, direct, "yaml was:\n{yaml}");
    // The digits are never reformatted.
    for digits in [
        "12345678901234567890123",
        "0.1",
        "1e-400",
        "1.0",
        "1E+5",
        "-0",
        "2.50",
    ] {
        assert!(yaml.contains(digits), "{digits} in\n{yaml}");
        assert!(back.contains(digits), "{digits} in\n{back}");
    }
    // Strings that YAML would read as something else stay strings.
    assert!(yaml.contains("zip: '02134'"), "{yaml}");
    assert!(yaml.contains("looks: 'yes'"), "{yaml}");
    assert!(yaml.contains("version: 0.6.2"), "{yaml}");
    assert!(
        yaml.contains("multi: |\n  line one\n  line two\n"),
        "{yaml}"
    );
}

#[test]
fn key_order_is_kept_everywhere() {
    let json = r#"{"z": 1, "a": 2, "m": {"y": 1, "b": 2}}"#;
    for to in [Lang::Yaml, Lang::Toml] {
        let out = ok(json, Lang::Json, to);
        let back = ok(&out, to, Lang::Json);
        assert_eq!(
            ok(&back, Lang::Json, Lang::Json),
            ok(json, Lang::Json, Lang::Json),
            "{to:?}:\n{out}"
        );
    }
}

#[test]
fn toml_keeps_order_with_tables_among_keys() {
    // A table with keys after it can't be a [header]: those keys would
    // land in it. It is written with dotted keys instead.
    let json = r#"{"a": 1, "server": {"host": "x", "port": 80}, "b": 2, "db": {"user": "u"}}"#;
    let toml = ok(json, Lang::Json, Lang::Toml);
    assert_eq!(
        toml,
        "a = 1\nserver = { host = \"x\", port = 80 }\nb = 2\n\n[db]\nuser = \"u\"\n"
    );
    let back = ok(&toml, Lang::Toml, Lang::Json);
    assert_eq!(back, ok(json, Lang::Json, Lang::Json));
}

#[test]
fn toml_round_trips_a_cargo_manifest() {
    let toml = r#"# A manifest
[package]
name = "convkit"
version = "0.6.2"
edition = "2021"
authors = ["shdwfruit"]

[dependencies]
serde = { version = "1", features = ["derive"] }
toml_edit = { version = "0.25", default-features = false }

[[bin]]
name = "conv"
path = "src/main.rs"

[[bin]]
name = "other"

[profile.dist]
inherits = "release"
lto = "thin"
"#;
    let (json, report) = conv(toml, Lang::Toml, Lang::Json).unwrap();
    assert_eq!(report.comments, 1);
    assert!(
        report.notes.iter().any(|n| n.contains("comment")),
        "{:?}",
        report.notes
    );
    let back = ok(&json, Lang::Json, Lang::Toml);
    // A table of tables comes back as [headers]: the model keeps what a
    // value is, not how it was spelled.
    assert_eq!(
        back,
        "[package]\nname = \"convkit\"\nversion = \"0.6.2\"\nedition = \"2021\"\nauthors = [\"shdwfruit\"]\n\n\
         [dependencies.serde]\nversion = \"1\"\nfeatures = [\"derive\"]\n\n[dependencies.toml_edit]\nversion = \"0.25\"\ndefault-features = false\n\n\
         [[bin]]\nname = \"conv\"\npath = \"src/main.rs\"\n\n[[bin]]\nname = \"other\"\n\n[profile.dist]\ninherits = \"release\"\nlto = \"thin\"\n"
    );
}

#[test]
fn toml_tables_among_keys_are_inline_when_short() {
    let toml = "[dependencies]\nclap = { version = \"4\", features = [\"derive\"] }\nwild = \"2\"\n\n[package]\nversion.workspace = true\nname = \"x\"\n";
    let json = ok(toml, Lang::Toml, Lang::Json);
    assert_eq!(ok(&json, Lang::Json, Lang::Toml), toml);
    // Too long for a line, it is dotted keys.
    let long = format!(r#"{{"a": {{"b": "{}", "c": 1}}, "d": 2}}"#, "x".repeat(120));
    let out = ok(&long, Lang::Json, Lang::Toml);
    assert!(out.starts_with("a.b = \""), "{out}");
}

#[test]
fn toml_numbers_and_dates_keep_their_text() {
    let toml = "a = 1_000\nb = 0xDEAD_BEEF\nc = 6.626e-34\nd = +inf\ne = 1979-05-27T07:32:00Z\nf = 07:32:00\ng = 0.1\n";
    let yaml = ok(toml, Lang::Toml, Lang::Yaml);
    assert_eq!(
        yaml,
        "a: 1000\nb: 3735928559\nc: 6.626e-34\nd: .inf\ne: 1979-05-27T07:32:00Z\nf: '07:32:00'\ng: 0.1\n"
    );
    // Back to TOML, the date-time is a date-time again.
    let back = ok(&yaml, Lang::Yaml, Lang::Toml);
    assert!(back.contains("e = 1979-05-27T07:32:00Z\n"), "{back}");
    // JSON has neither infinity nor dates.
    let e = conv(toml, Lang::Toml, Lang::Json).unwrap_err();
    assert!(
        e.message.contains("JSON has no infinity, and d is inf"),
        "{e:?}"
    );
    let (json, report) = conv("e = 1979-05-27\n", Lang::Toml, Lang::Json).unwrap();
    assert_eq!(json, "{\n  \"e\": \"1979-05-27\"\n}\n");
    assert!(report.notes.iter().any(|n| n.contains("no date type")));
}

#[test]
fn unicode_survives_every_format() {
    let json = r#"{"ключ": "значение", "日本": "語", "emoji 🎉": "👍🏽", "combining": "é", "rtl": "שלום", "nul": "a\u0000b", "bom": "\ufeff"}"#;
    for to in [Lang::Yaml, Lang::Toml] {
        let out = ok(json, Lang::Json, to);
        let back = ok(&out, to, Lang::Json);
        assert_eq!(back, ok(json, Lang::Json, Lang::Json), "{to:?}:\n{out}");
    }
    let csv = ok(&format!("[{json}]"), Lang::Json, Lang::Csv);
    assert!(
        csv.starts_with("ключ,日本,emoji 🎉,combining,rtl,nul,bom\n"),
        "{csv}"
    );
}

#[test]
fn yaml_reads_by_the_core_schema() {
    let yaml = "a: 0x1F\nb: 0o17\nc: +12\nd: .5\ne: ~\nf: Null\ng: TRUE\nh: 1_000\ni: '12'\nj: !!str 12\nk: !!float 3\nl: 2001-12-14\n";
    let json = ok(yaml, Lang::Yaml, Lang::Json);
    assert_eq!(
        json,
        "{\n  \"a\": 31,\n  \"b\": 15,\n  \"c\": 12,\n  \"d\": 0.5,\n  \"e\": null,\n  \"f\": null,\n  \"g\": true,\n  \"h\": \"1_000\",\n  \"i\": \"12\",\n  \"j\": \"12\",\n  \"k\": 3.0,\n  \"l\": \"2001-12-14\"\n}\n"
    );
}

#[test]
fn yaml_aliases_and_merge_keys_are_expanded() {
    let yaml = "base: &b {x: 1, y: 2}\nuse:\n  <<: *b\n  y: 3\n  z: 4\nlist: [*b, *b]\n";
    let (json, report) = conv(yaml, Lang::Yaml, Lang::Json).unwrap();
    assert_eq!(
        json,
        "{\n  \"base\": {\n    \"x\": 1,\n    \"y\": 2\n  },\n  \"use\": {\n    \"x\": 1,\n    \"y\": 3,\n    \"z\": 4\n  },\n  \"list\": [\n    {\n      \"x\": 1,\n      \"y\": 2\n    },\n    {\n      \"x\": 1,\n      \"y\": 2\n    }\n  ]\n}\n"
    );
    assert!(
        report.notes.iter().any(|n| n.contains("Merge keys")),
        "{:?}",
        report.notes
    );
}

#[test]
fn a_billion_laughs_is_stopped() {
    let mut yaml = String::from("a0: &a0 [lol, lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
    for i in 1..10 {
        let refs = vec![format!("*a{}", i - 1); 10].join(", ");
        yaml.push_str(&format!("a{i}: &a{i} [{refs}]\n"));
    }
    let e = conv(&yaml, Lang::Yaml, Lang::Json).unwrap_err();
    assert_eq!(e.kind, DataErrorKind::TooBig);
    assert!(e.message.contains("aliases"), "{e:?}");
    assert!(
        e.hint.as_deref().unwrap().contains("billion laughs"),
        "{e:?}"
    );
}

#[test]
fn yaml_documents_become_a_list_or_are_refused_by_toml() {
    let yaml = "---\na: 1\n---\na: 2\n";
    let (json, report) = conv(yaml, Lang::Yaml, Lang::Json).unwrap();
    assert_eq!(
        json,
        "[\n  {\n    \"a\": 1\n  },\n  {\n    \"a\": 2\n  }\n]\n"
    );
    assert!(
        report.notes[0].contains("2 YAML documents"),
        "{:?}",
        report.notes
    );
    // YAML to YAML keeps them as documents.
    assert_eq!(ok(yaml, Lang::Yaml, Lang::Yaml), "a: 1\n---\na: 2\n");
    let e = conv(yaml, Lang::Yaml, Lang::Toml).unwrap_err();
    assert!(e.message.contains("one document"), "{e:?}");
}

#[test]
fn yaml_tags_are_refused_strictly_and_dropped_leniently() {
    let yaml = "Bucket:\n  Name: !Ref Name\n  Arn: !GetAtt [B, Arn]\n";
    let e = strict_err(yaml, Lang::Yaml);
    assert_eq!(at(&e), (2, 9));
    assert!(e.message.contains("!Ref"), "{e:?}");
    let (json, report) = conv(yaml, Lang::Yaml, Lang::Json).unwrap();
    assert!(json.contains("\"Name\": \"Name\""), "{json}");
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("!Ref and !GetAtt were dropped")),
        "{:?}",
        report.notes
    );
}

#[test]
fn yaml_one_one_booleans_are_noted() {
    let (json, report) = conv("enabled: yes\non: push\n", Lang::Yaml, Lang::Json).unwrap();
    assert_eq!(json, "{\n  \"enabled\": \"yes\",\n  \"on\": \"push\"\n}\n");
    assert!(report.notes.iter().any(|n| n.contains("YAML 1.1")));
    // As a key, `on` means nothing else, and says nothing.
    let (_, report) = conv("on: push\n", Lang::Yaml, Lang::Json).unwrap();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

#[test]
fn github_actions_yaml_round_trips() {
    let yaml = "name: CI\non:\n  push:\n    branches: [main]\njobs:\n  test:\n    runs-on: ${{ matrix.os }}\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          cargo fmt --all -- --check\n          cargo test # all of it\n        env:\n          CARGO_TERM_COLOR: always\n";
    let (out, report) = conv(yaml, Lang::Yaml, Lang::Yaml).unwrap();
    assert_eq!(
        out,
        "name: CI\non:\n  push:\n    branches:\n      - main\njobs:\n  test:\n    runs-on: ${{ matrix.os }}\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          cargo fmt --all -- --check\n          cargo test # all of it\n        env:\n          CARGO_TERM_COLOR: always\n"
    );
    assert_eq!(report.comments, 0, "the # is inside the block");
    let json = ok(yaml, Lang::Yaml, Lang::Json);
    assert_eq!(ok(&json, Lang::Json, Lang::Yaml), out);
}

#[test]
fn indent_tab_minify_and_sort_keys() {
    let json = r#"{"b": [1, {"d": 1, "c": 2}], "a": {}}"#;
    let opts = |f: fn(&mut Options)| {
        let mut o = Options::new("in");
        f(&mut o);
        o
    };
    let min = run_with(json, Lang::Json, Lang::Json, &opts(|o| o.minify = true))
        .unwrap()
        .0;
    assert_eq!(min, "{\"b\":[1,{\"d\":1,\"c\":2}],\"a\":{}}\n");
    let sorted = run_with(
        json,
        Lang::Json,
        Lang::Json,
        &opts(|o| {
            o.minify = true;
            o.sort_keys = true;
        }),
    )
    .unwrap()
    .0;
    assert_eq!(sorted, "{\"a\":{},\"b\":[1,{\"c\":2,\"d\":1}]}\n");
    let tab = run_with(
        json,
        Lang::Json,
        Lang::Json,
        &opts(|o| o.indent = Some(Indent::Tab)),
    )
    .unwrap()
    .0;
    assert_eq!(tab, "{\n\t\"b\": [\n\t\t1,\n\t\t{\n\t\t\t\"d\": 1,\n\t\t\t\"c\": 2\n\t\t}\n\t],\n\t\"a\": {}\n}\n");
    let four = run_with(
        json,
        Lang::Json,
        Lang::Yaml,
        &opts(|o| o.indent = Some(Indent::Spaces(4))),
    )
    .unwrap()
    .0;
    assert_eq!(four, "b:\n    - 1\n    - d: 1\n      c: 2\na: {}\n");
    for (to, f) in [
        (
            Lang::Yaml,
            (|o: &mut Options| o.minify = true) as fn(&mut Options),
        ),
        (Lang::Yaml, |o| o.indent = Some(Indent::Tab)),
        (Lang::Toml, |o| o.indent = Some(Indent::Spaces(4))),
    ] {
        let e = run_with(json, Lang::Json, to, &opts(f)).unwrap_err();
        assert_eq!(e.kind, DataErrorKind::Unrepresentable, "{e:?}");
    }
}

#[test]
fn json_errors_say_where_and_why() {
    let cases: &[(&str, (usize, usize), &str, &str)] = &[
        (
            "{\n  \"a\": 1,\n}",
            (2, 9),
            "a comma after the last item",
            "JSON allows no comma",
        ),
        (
            "{'a': 1}",
            (1, 2),
            "a key in single quotes",
            "double quotes",
        ),
        (
            "{\"a\": 'x'}",
            (1, 7),
            "a string in single quotes",
            "double quotes",
        ),
        (
            "{\n  // hi\n  \"a\": 1\n}",
            (2, 3),
            "a comment",
            "JSON has no comments",
        ),
        ("{a: 1}", (1, 2), "the key a isn't in quotes", "\"a\""),
        ("[1 2]", (1, 4), "a comma is missing before this", "commas"),
        (
            "{\"a\": 1\n\"b\": 2}",
            (2, 1),
            "a comma is missing before this",
            "commas",
        ),
        (
            "{\"a\": [1, 2}",
            (1, 12),
            "this } doesn't match the list opened at line 1, column 7",
            "]",
        ),
        (
            "{\"a\": 1",
            (1, 8),
            "ends before the object opened at line 1, column 1",
            "}",
        ),
        ("[\"abc", (1, 2), "this string is never closed", "\""),
        ("[\"a\nb\"]", (1, 4), "the line ends inside a string", "\\n"),
        ("{\"a\": True}", (1, 7), "True isn't JSON", "true"),
        ("{\"a\": None}", (1, 7), "None isn't JSON", "null"),
        ("[01]", (1, 2), "01 starts with a 0", "\"01\""),
        ("[.5]", (1, 2), "no digit before the point", "0.5"),
        ("[NaN]", (1, 2), "JSON has no NaN", "YAML"),
        (
            "{\"a\": 1, \"a\": 2}",
            (1, 10),
            "the key \"a\" is in this object twice",
            "line 1, column 2",
        ),
        (
            "{\"a\" = 1}",
            (1, 6),
            "an = between a key and its value",
            ":",
        ),
        (
            "{\"a\": 1}\n{\"b\": 2}",
            (2, 1),
            "more text after the JSON value ended",
            "JSON Lines",
        ),
        ("", (1, 1), "the text is empty", "{}"),
        (
            "[1,,2]",
            (1, 4),
            "a value is missing before this comma",
            "null",
        ),
        (
            "{\"a\": hello}",
            (1, 7),
            "the text hello isn't in quotes",
            "\"hello\"",
        ),
        ("[\"\\x41\"]", (1, 3), "\\x isn't a JSON escape", "\\uXXXX"),
        (
            "[\"\\ud83d\"]",
            (1, 3),
            "\\uD83D is half of a character",
            "other half",
        ),
    ];
    for (text, pos, message, hint) in cases {
        let e = strict_err(text, Lang::Json);
        assert_eq!(at(&e), *pos, "{text:?}: {e:?}");
        assert!(e.message.contains(message), "{text:?}: {e:?}");
        assert!(
            e.hint.as_deref().unwrap_or("").contains(hint),
            "{text:?}: {e:?}"
        );
    }
}

#[test]
fn the_snippet_points_at_the_spot() {
    let e = strict_err(
        "{\n  \"name\": \"x\",\n  \"tags\": [\"a\",],\n}",
        Lang::Json,
    );
    assert_eq!(at(&e), (3, 15));
    assert_eq!(
        e.snippet_lines(),
        vec![" 3 |   \"tags\": [\"a\",],", "   |               ^"]
    );
    let c = e.into_conv_error("data.json");
    assert_eq!(c.code, crate::ErrorCode::InvalidData);
    assert!(
        c.message
            .starts_with("data.json: line 3, column 15: a comma after the last item\n   3 | "),
        "{}",
        c.message
    );
}

#[test]
fn lenient_reading_takes_json5_and_says_so() {
    let text = "// settings\n{\n  name: 'conv', /* inline */\n  hex: 0x1F,\n  plus: +1,\n  half: .5,\n  list: [1, 2,],\n  inf: Infinity,\n}\n";
    let (yaml, report) = conv(text, Lang::Json, Lang::Yaml).unwrap();
    assert_eq!(
        yaml,
        "name: conv\nhex: 31\nplus: 1\nhalf: 0.5\nlist:\n  - 1\n  - 2\ninf: .inf\n"
    );
    assert_eq!(report.comments, 2);
    let notes = report.notes.join("\n");
    assert!(notes.contains("in is JSON5, not JSON: it has trailing commas, single-quoted strings, unquoted keys and JSON5 numbers (the first at line 3, column 3)"), "{notes}");
    assert!(
        notes.contains("The 2 comments in in are not kept; JSON has none."),
        "{notes}"
    );
}

#[test]
fn yaml_errors_say_where_and_why() {
    let e = strict_err("a: 1\nb:\n\t- x\n", Lang::Yaml);
    assert_eq!(e.line, Some(3), "{e:?}");
    assert!(e.hint.as_deref().unwrap().contains("spaces"), "{e:?}");
    let e = strict_err("a: 1\nb: 2\na: 3\n", Lang::Yaml);
    assert_eq!(at(&e), (3, 1));
    assert!(
        e.message.contains("\"a\" is in this mapping twice"),
        "{e:?}"
    );
    assert!(
        e.hint.as_deref().unwrap().contains("line 1, column 1"),
        "{e:?}"
    );
    let e = strict_err("key: value: other\n", Lang::Yaml);
    assert_eq!(e.line, Some(1));
    assert!(e.hint.as_deref().unwrap().contains("quotes"), "{e:?}");
    let e = strict_err("a: *missing\n", Lang::Yaml);
    assert_eq!(e.line, Some(1), "{e:?}");
}

#[test]
fn toml_errors_say_where() {
    let e = strict_err("[package]\nname = conv\n", Lang::Toml);
    assert_eq!(at(&e), (2, 8), "{e:?}");
    assert!(
        e.hint.as_deref().unwrap_or("").contains("\"conv\""),
        "{e:?}"
    );
    let e = strict_err("a = 1\na = 2\n", Lang::Toml);
    assert_eq!(e.line, Some(2), "{e:?}");
}

#[test]
fn toml_refuses_what_it_cannot_hold() {
    let e = conv(r#"{"a": {"b": [1, null]}}"#, Lang::Json, Lang::Toml).unwrap_err();
    assert_eq!(e.message, "TOML has no null, and a.b[1] is null");
    let e = conv("[1, 2]", Lang::Json, Lang::Toml).unwrap_err();
    assert!(e.message.contains("this is a list"), "{e:?}");
    assert!(
        e.hint.as_deref().unwrap().contains("no top-level list"),
        "{e:?}"
    );
    let e = conv(r#"{"id": 12345678901234567890}"#, Lang::Json, Lang::Toml).unwrap_err();
    assert!(
        e.message
            .contains("64 bits, and id is 12345678901234567890"),
        "{e:?}"
    );
    assert!(ok(r#"{"id": 9223372036854775807}"#, Lang::Json, Lang::Toml)
        .contains("9223372036854775807"));
}

#[test]
fn json_refuses_nan_and_infinity_with_the_path() {
    let e = conv("x: [1, .nan]\n", Lang::Yaml, Lang::Json).unwrap_err();
    assert_eq!(e.message, "JSON has no NaN, and x[1] is nan");
}

#[test]
fn csv_columns_are_typed_whole() {
    let csv = "id,zip,price,ok,note,blank\n1,02134,1.50,TRUE,hi,\n2,10001,,false,,\n3,94105,3,true,there,\n";
    let (json, report) = conv(csv, Lang::Csv, Lang::Json).unwrap();
    assert_eq!(
        json,
        "[\n  {\n    \"id\": 1,\n    \"zip\": \"02134\",\n    \"price\": 1.50,\n    \"ok\": true,\n    \"note\": \"hi\",\n    \"blank\": \"\"\n  },\n  {\n    \"id\": 2,\n    \"zip\": \"10001\",\n    \"price\": null,\n    \"ok\": false,\n    \"note\": \"\",\n    \"blank\": \"\"\n  },\n  {\n    \"id\": 3,\n    \"zip\": \"94105\",\n    \"price\": 3,\n    \"ok\": true,\n    \"note\": \"there\",\n    \"blank\": \"\"\n  }\n]\n"
    );
    let notes = report.notes.join("\n");
    assert!(notes.contains("zip codes"), "{notes}");
    assert!(notes.contains("became null"), "{notes}");
}

#[test]
fn csv_round_trips_through_json() {
    let csv = "name,qty,\"note, with comma\"\nwidget,3,\"say \"\"hi\"\"\"\n\"multi\nline\",4,x\n";
    let json = ok(csv, Lang::Csv, Lang::Json);
    let back = ok(&json, Lang::Json, Lang::Csv);
    assert_eq!(back, csv);
}

#[test]
fn csv_headers_are_named_when_blank_or_repeated() {
    let (json, report) = conv("a,,a\n1,2,3\n", Lang::Csv, Lang::Json).unwrap();
    assert_eq!(
        json,
        "[\n  {\n    \"a\": 1,\n    \"column2\": 2,\n    \"a_2\": 3\n  }\n]\n"
    );
    assert_eq!(report.notes.len(), 2, "{:?}", report.notes);
}

#[test]
fn csv_rows_longer_than_the_header_are_refused() {
    let e = conv("a,b\n1,2,3\n", Lang::Csv, Lang::Json).unwrap_err();
    assert_eq!(e.line, Some(2));
    // Trailing empty cells, as some exports write, are fine.
    assert!(conv("a,b\n1,2,,\n", Lang::Csv, Lang::Json).is_ok());
    // A short row's missing cells are empty.
    let json = ok("a,b\n1\n", Lang::Csv, Lang::Json);
    assert!(json.contains("\"b\": \"\""), "{json}");
}

#[test]
fn csv_encodings_and_delimiters_are_read() {
    let mut cp1252 = b"name;price\n".to_vec();
    cp1252.extend_from_slice(b"caf\xe9;1,5\n");
    let (json, report) = run_bytes(&cp1252, Lang::Csv, Lang::Json);
    assert_eq!(
        json,
        "[\n  {\n    \"name\": \"café\",\n    \"price\": 1.5\n  }\n]\n"
    );
    assert!(report.notes.iter().any(|n| n.contains("Windows-1252")));
    assert!(report.notes.iter().any(|n| n.contains("decimal commas")));

    let mut utf16 = vec![0xFF, 0xFE];
    for u in "a\tb\r\n1\tü\r\n".encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    let (json, _) = run_bytes(&utf16, Lang::Csv, Lang::Json);
    assert_eq!(json, "[\n  {\n    \"a\": 1,\n    \"b\": \"ü\"\n  }\n]\n");
}

fn run_bytes(bytes: &[u8], from: Lang, to: Lang) -> (String, Report) {
    let opts = Options {
        lenient: true,
        ..Options::new("in")
    };
    let mut out = Plain(Vec::new());
    let report = transform(&Source::Text(bytes), from, to, &opts, &mut out).unwrap();
    (String::from_utf8(out.0).unwrap(), report)
}

#[test]
fn json_lists_flatten_into_dotted_columns() {
    let json = r#"[{"id": 1, "user": {"name": "a", "tags": ["x", "y"]}, "meta": {}},
                  {"id": 2, "extra": true, "user": {"name": "=SUM(A1)"}}]"#;
    let (csv, report) = conv(json, Lang::Json, Lang::Csv).unwrap();
    assert_eq!(
        csv,
        "id,user.name,user.tags.0,user.tags.1,meta,extra\n1,a,x,y,,\n2,=SUM(A1),,,,true\n"
    );
    let notes = report.notes.join("\n");
    assert!(notes.contains("address.city"), "{notes}");
    assert!(notes.contains("Empty lists and objects"), "{notes}");
    assert!(notes.contains("formula"), "{notes}");
    // A list of lists is rows as they are.
    assert_eq!(
        ok("[[\"a\", \"b\"], [1, null]]", Lang::Json, Lang::Csv),
        "a,b\n1,\n"
    );
}

#[test]
fn csv_refuses_what_a_row_cannot_hold() {
    let e = conv(r#"{"a": 1}"#, Lang::Json, Lang::Csv).unwrap_err();
    assert!(
        e.message
            .contains("a CSV is a list of rows, and this is an object"),
        "{e:?}"
    );
    let e = conv(r#"[{"a.b": 1, "a": {"b": 2}}]"#, Lang::Json, Lang::Csv).unwrap_err();
    assert!(
        e.message.contains("two values for the column \"a.b\""),
        "{e:?}"
    );
    let e = conv(r#"[{"a": 1}, [1]]"#, Lang::Json, Lang::Csv).unwrap_err();
    assert!(
        e.message
            .contains("item 2 of the list is a list, and the rows before it are objects"),
        "{e:?}"
    );
    let e = conv("[1, 2]", Lang::Json, Lang::Csv).unwrap_err();
    assert_eq!(
        e.message,
        "item 1 of the list is a number, and a CSV row is an object or a list"
    );
    let e = conv("a = 1\n", Lang::Toml, Lang::Csv).unwrap_err();
    assert!(e.message.contains("TOML has no top-level list"), "{e:?}");
}

#[test]
fn what_streams_never_counts_against_memory() {
    let rows: Vec<String> = (0..2000)
        .map(|i| format!(r#"{{"id": {i}, "name": "row {i}", "nested": {{"x": [{i}, {i}]}}}}"#))
        .collect();
    let json = format!("[{}]", rows.join(",\n"));
    let csv = ok(&json, Lang::Json, Lang::Csv);
    TEST_LIMIT.with(|l| l.set(Some(64 * 1024)));
    let streamed = [
        conv(&json, Lang::Json, Lang::Json),
        conv(&json, Lang::Json, Lang::Yaml),
        conv(&json, Lang::Json, Lang::Csv),
        conv(&csv, Lang::Csv, Lang::Json),
        conv(&csv, Lang::Csv, Lang::Yaml),
    ];
    let held = [
        conv(&json, Lang::Json, Lang::Toml),
        conv(&ok(&json, Lang::Json, Lang::Yaml), Lang::Yaml, Lang::Json),
    ];
    let sorted = run_with(
        &json,
        Lang::Json,
        Lang::Json,
        &Options {
            sort_keys: true,
            ..Options::new("in")
        },
    );
    TEST_LIMIT.with(|l| l.set(None));
    for r in streamed {
        assert!(r.is_ok(), "{:?}", r.err());
    }
    for r in held.into_iter().chain([sorted]) {
        let e = r.unwrap_err();
        assert_eq!(e.kind, DataErrorKind::TooBig, "{e:?}");
        assert!(
            e.hint.as_deref().unwrap().contains("stream at any size"),
            "{e:?}"
        );
    }
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let deep = "[".repeat(100_000);
    let e = strict_err(&deep, Lang::Json);
    assert_eq!(e.kind, DataErrorKind::TooBig);
    let yaml = "- ".repeat(5_000) + "x\n";
    assert!(conv(&yaml, Lang::Yaml, Lang::Json).is_err());
}

#[test]
fn utf16_json_is_read() {
    let mut bytes = vec![0xFF, 0xFE];
    for u in "{\"a\": \"ü\"}".encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let (json, report) = run_bytes(&bytes, Lang::Json, Lang::Yaml);
    assert_eq!(json, "a: ü\n");
    assert!(report.notes.iter().any(|n| n.contains("UTF-16")));
}

#[test]
fn sniffing_tells_the_three_apart() {
    assert_eq!(sniff(b"{\"a\": 1}"), Lang::Json);
    assert_eq!(sniff(b"  [1, 2]"), Lang::Json);
    assert_eq!(sniff(b"// c\n{}"), Lang::Json);
    assert_eq!(sniff(b"[package]\nname = \"x\""), Lang::Toml);
    assert_eq!(sniff(b"[[bin]]\nname = \"x\""), Lang::Toml);
    assert_eq!(sniff(b"# comment\nname = \"x\""), Lang::Toml);
    assert_eq!(sniff(b"name: x\nlist: [a = b]"), Lang::Yaml);
    assert_eq!(sniff(b"- a\n- b"), Lang::Yaml);
    assert_eq!(sniff(b"---\na: 1"), Lang::Yaml);
}

#[test]
fn pairs_and_refusals_are_named() {
    assert!(converts(Lang::Json, Lang::Yaml));
    assert!(converts(Lang::Csv, Lang::Yaml));
    assert!(!converts(Lang::Toml, Lang::Csv));
    assert!(!converts(Lang::Json, Lang::Json));
    assert!(why_not(Format::Json, Format::Json)
        .unwrap()
        .contains("conv fmt"));
    assert!(why_not(Format::Csv, Format::Toml)
        .unwrap()
        .contains("no top-level list"));
    assert_eq!(why_not(Format::Json, Format::Yaml), None);
}

#[test]
fn yaml_writer_quotes_keys_and_handles_odd_strings() {
    let json = r#"{"": 1, "a: b": 2, "- x": 3, "123": 4, "line\nbreak": 5, "ok": "tab\there", "lead": " x", "trail": "x ", "nl": "\n", "dash": "-"}"#;
    let yaml = ok(json, Lang::Json, Lang::Yaml);
    assert_eq!(
        yaml,
        "'': 1\n'a: b': 2\n'- x': 3\n'123': 4\n\"line\\nbreak\": 5\nok: \"tab\\there\"\nlead: ' x'\ntrail: 'x '\nnl: \"\\n\"\ndash: '-'\n"
    );
    assert_eq!(
        ok(&yaml, Lang::Yaml, Lang::Json),
        ok(json, Lang::Json, Lang::Json)
    );
}

#[test]
fn empty_and_scalar_documents() {
    assert_eq!(ok("", Lang::Yaml, Lang::Json), "null\n");
    assert_eq!(ok("42", Lang::Json, Lang::Yaml), "42\n");
    assert_eq!(ok("[]", Lang::Json, Lang::Yaml), "[]\n");
    assert_eq!(ok("{}", Lang::Json, Lang::Toml), "");
    assert_eq!(ok("\"a\nb\"", Lang::Yaml, Lang::Yaml), "a b\n");
}
