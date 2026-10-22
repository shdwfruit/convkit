//! Puts the changelog into the binary for `conv changelog`.
//!
//! In the workspace it is CHANGELOG.md at the root. `crates/conv/CHANGELOG.md`
//! links to it, and `cargo package` turns the link into a copy, so a crate
//! built from crates.io has it too. The root is read first because a
//! Windows checkout without symlinks makes the link a text file holding its
//! path.

use std::path::Path;

fn main() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = here.join("../..");
    let workspace = root.join("Cargo.toml").is_file() && root.join("CHANGELOG.md").is_file();
    let source = if workspace {
        root.join("CHANGELOG.md")
    } else {
        here.join("CHANGELOG.md")
    };
    println!("cargo:rerun-if-changed={}", source.display());
    let text = std::fs::read_to_string(&source)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", source.display()));
    let out =
        Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("CHANGELOG.md");
    std::fs::write(&out, text).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}
