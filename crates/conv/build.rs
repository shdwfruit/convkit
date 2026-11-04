//! Puts the changelog into the binary for `conv changelog`, and the
//! version `conv --version` prints, and gives the binary the stack it
//! needs on Windows.
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

    // scripts/dev-install.sh sets the branch and commit, so a dev build
    // says what it was built from: `0.6.1 (feat/copy-paste ce31260)`.
    println!("cargo:rerun-if-env-changed=CONV_DEV_STAMP");
    let version = env!("CARGO_PKG_VERSION");
    let version = match std::env::var("CONV_DEV_STAMP") {
        Ok(stamp) if !stamp.is_empty() => format!("{version} ({stamp})"),
        _ => version.to_string(),
    };
    println!("cargo:rustc-env=CONV_VERSION={version}");

    // Windows gives a program's main thread 1 MB of stack, where Linux and
    // macOS give 8 MB. A debug build parsing conv's flags and commands
    // outgrew 1 MB and overflowed before doing anything, so conv asks for
    // the same 8 MB on Windows.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" {
        match target_env.as_str() {
            "msvc" => println!("cargo:rustc-link-arg-bins=/STACK:8388608"),
            "gnu" => println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608"),
            _ => {}
        }
    }
}
