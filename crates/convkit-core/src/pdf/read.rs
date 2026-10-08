//! What conv needs to know about a PDF before merging or splitting it, from
//! one `qpdf --json` call.

use std::path::{Path, PathBuf};

use super::{display_name, path_arg, qpdf_reason};
use crate::error::{ConvError, ErrorCode, Result};
use crate::procutil::backend_command;

/// What `merge` and `split` need to know about one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfInfo {
    pub path: PathBuf,
    pub pages: u32,
    pub has_bookmarks: bool,
    /// Encrypted with permission restrictions but no open password: qpdf
    /// opens it, and conv's output drops the restrictions.
    pub restricted: bool,
    /// qpdf had to repair it (exit status 3).
    pub damaged: bool,
}

/// The qpdf arguments that read `input`: the three JSON keys conv uses.
pub fn read_args(input: &Path) -> Vec<String> {
    vec![
        "--json".to_string(),
        "--json-key=encrypt".to_string(),
        "--json-key=outlines".to_string(),
        "--json-key=pages".to_string(),
        path_arg(input),
    ]
}

fn qpdf_error(message: String) -> ConvError {
    ConvError {
        backend: Some(crate::Backend::Qpdf),
        ..ConvError::new(ErrorCode::ConversionFailed, message)
    }
}

/// Reads `input` with the qpdf at `qpdf`.
pub fn read(qpdf: &Path, input: &Path) -> Result<PdfInfo> {
    let out = backend_command(qpdf)
        .args(read_args(input))
        .output()
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("failed to run {}: {e}", qpdf.display()),
            )
        })?;
    parse_read(
        input,
        out.status.code(),
        &out.stdout,
        &String::from_utf8_lossy(&out.stderr),
    )
}

/// Turns one finished `qpdf --json` read into a `PdfInfo`. qpdf exits 0 on
/// success, 3 when it succeeded but had to repair the file, and 2 when it
/// could not open it at all.
pub fn parse_read(input: &Path, code: Option<i32>, stdout: &[u8], stderr: &str) -> Result<PdfInfo> {
    let name = display_name(input);
    let unreadable =
        |reason: &str| qpdf_error(format!("{name} could not be read as a PDF: {reason}"));
    match code {
        Some(0) | Some(3) => {}
        Some(2) if stderr.contains("invalid password") => {
            return Err(qpdf_error(format!(
                "{name} is password-protected, and conv can't open it yet"
            )));
        }
        _ => {
            let reason = qpdf_reason(stderr);
            let prefix = format!("{}: ", path_arg(input));
            return Err(unreadable(reason.strip_prefix(&prefix).unwrap_or(&reason)));
        }
    }
    let v: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|e| unreadable(&format!("qpdf's report was not JSON ({e})")))?;
    let pages = v["pages"]
        .as_array()
        .map(|a| a.len() as u32)
        .ok_or_else(|| unreadable("qpdf's report has no page list"))?;
    let has_bookmarks = v["outlines"].as_array().is_some_and(|a| !a.is_empty());
    let enc = &v["encrypt"];
    let restricted = enc["encrypted"].as_bool() == Some(true)
        && enc["ownerpasswordmatched"].as_bool() != Some(true);
    Ok(PdfInfo {
        path: input.to_path_buf(),
        pages,
        has_bookmarks,
        restricted,
        damaged: code == Some(3),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(encrypted: bool, owner_matched: bool, outlines: usize, pages: usize) -> Vec<u8> {
        let outlines: Vec<_> = (0..outlines)
            .map(|i| serde_json::json!({"title": format!("b{i}"), "kids": []}))
            .collect();
        let pages: Vec<_> = (0..pages)
            .map(
                |i| serde_json::json!({"object": format!("{} 0 R", i + 10), "pageposfrom1": i + 1}),
            )
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "parameters": {"decodelevel": "generalized"},
            "pages": pages,
            "encrypt": {
                "encrypted": encrypted,
                "userpasswordmatched": true,
                "ownerpasswordmatched": owner_matched
            },
            "outlines": outlines
        }))
        .unwrap()
    }

    #[test]
    fn a_plain_pdf() {
        let info =
            parse_read(Path::new("a.pdf"), Some(0), &report(false, false, 0, 3), "").unwrap();
        assert_eq!(
            info,
            PdfInfo {
                path: PathBuf::from("a.pdf"),
                pages: 3,
                has_bookmarks: false,
                restricted: false,
                damaged: false,
            }
        );
    }

    #[test]
    fn bookmarks_restrictions_and_repairs_are_noticed() {
        let info = parse_read(Path::new("a.pdf"), Some(0), &report(true, false, 2, 1), "").unwrap();
        assert!(info.has_bookmarks);
        assert!(
            info.restricted,
            "encrypted, opened without the owner password"
        );
        let info = parse_read(Path::new("a.pdf"), Some(0), &report(true, true, 0, 1), "").unwrap();
        assert!(
            !info.restricted,
            "the owner password matched, so nothing is restricted"
        );
        let info = parse_read(
            Path::new("a.pdf"),
            Some(3),
            &report(false, false, 0, 1),
            "WARNING: a.pdf: file is damaged\n",
        )
        .unwrap();
        assert!(info.damaged);
    }

    #[test]
    fn a_password_protected_pdf_is_refused() {
        let e = parse_read(
            Path::new("enc.pdf"),
            Some(2),
            b"",
            "qpdf: enc.pdf: invalid password\n",
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.message,
            "enc.pdf is password-protected, and conv can't open it yet"
        );
    }

    #[test]
    fn an_unreadable_file_is_refused_with_qpdfs_reason() {
        let stderr = "WARNING: fake.pdf: can't find PDF header\n\
                      qpdf: fake.pdf: unable to find trailer dictionary while recovering damaged file\n";
        let e = parse_read(Path::new("fake.pdf"), Some(2), b"", stderr).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConversionFailed);
        assert_eq!(
            e.message,
            "fake.pdf could not be read as a PDF: \
             unable to find trailer dictionary while recovering damaged file"
        );
    }

    #[test]
    fn output_that_is_not_json_is_an_error_not_a_panic() {
        let e = parse_read(Path::new("a.pdf"), Some(0), b"not json", "").unwrap_err();
        assert!(
            e.message.starts_with("a.pdf could not be read as a PDF"),
            "{}",
            e.message
        );
    }

    #[test]
    fn the_read_asks_for_exactly_three_keys() {
        assert_eq!(
            read_args(Path::new("-x.pdf")),
            vec![
                "--json",
                "--json-key=encrypt",
                "--json-key=outlines",
                "--json-key=pages",
                "./-x.pdf"
            ]
        );
    }
}
