//! What conv needs to know about a PDF before merging, splitting or
//! editing it, from one `qpdf --json` call, and opening one that needs a
//! password.

use std::path::{Path, PathBuf};

use super::lock::{Password, PASSWORD_ENV};
use super::{display_name, path_arg, qpdf_output, qpdf_reason};
use crate::error::{ConvError, ErrorCode, Remediation, Result};

/// What conv's PDF commands need to know about one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfInfo {
    pub path: PathBuf,
    pub pages: u32,
    pub has_bookmarks: bool,
    /// The pages bookmarks lead to, at any depth, in the order listed.
    pub bookmarked_pages: Vec<u32>,
    /// Encrypted at all: with a password to open it, or only restrictions.
    pub encrypted: bool,
    /// Restricts printing, copying or editing for whoever opened it: conv's
    /// output drops the restrictions.
    pub restricted: bool,
    /// The password that opened it, when it needed one.
    pub password: Option<Password>,
    /// qpdf had to repair it (exit status 3).
    pub damaged: bool,
}

impl PdfInfo {
    /// Needed a password to open.
    pub fn locked(&self) -> bool {
        self.password.is_some()
    }
}

/// Asked for a password when a PDF needs one: the file, and whether the
/// last password given for it was wrong. `None` gives up.
pub type AskPassword<'a> = dyn FnMut(&Path, bool) -> Option<Password> + 'a;

/// The qpdf arguments that read `input`: the three JSON keys conv uses,
/// after the password that opens it.
pub fn read_args(input: &Path, password: Option<&Password>, reveal: bool) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(p) = password {
        v.push(password_arg(p, reveal));
    }
    v.extend([
        "--json".to_string(),
        "--json-key=encrypt".to_string(),
        "--json-key=outlines".to_string(),
        "--json-key=pages".to_string(),
        path_arg(input),
    ]);
    v
}

/// `--password=...`, or `--password=<password>` for showing.
pub(crate) fn password_arg(p: &Password, reveal: bool) -> String {
    if reveal {
        format!("--password={}", p.expose())
    } else {
        "--password=<password>".to_string()
    }
}

fn qpdf_error(message: String) -> ConvError {
    ConvError {
        backend: Some(crate::Backend::Qpdf),
        ..ConvError::new(ErrorCode::ConversionFailed, message)
    }
}

/// `input` needs a password, and none was given.
pub fn password_needed(input: &Path) -> ConvError {
    ConvError {
        code: ErrorCode::PasswordProtected,
        message: format!("{} is password-protected", display_name(input)),
        backend: None,
        remediation: Some(Remediation {
            managed: None,
            manual: Some(format!(
                "run it in a terminal to be asked for the password, or set {PASSWORD_ENV}"
            )),
        }),
    }
}

/// The password given for `input` does not open it.
pub fn wrong_password(input: &Path) -> ConvError {
    ConvError {
        code: ErrorCode::PasswordProtected,
        message: format!("wrong password for {}", display_name(input)),
        backend: None,
        remediation: None,
    }
}

/// Reads `input` with the qpdf at `qpdf`, opening it with `password`.
pub fn read(qpdf: &Path, input: &Path, password: Option<&Password>) -> Result<PdfInfo> {
    let out = qpdf_output(
        qpdf,
        &read_args(input, password, true),
        password.is_some(),
        None,
    )
    .map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("failed to run {}: {e}", qpdf.display()),
        )
    })?;
    let mut info = parse_read(
        input,
        out.status.code(),
        &out.stdout,
        &String::from_utf8_lossy(&out.stderr),
    )?;
    info.password = password.cloned();
    Ok(info)
}

/// Reads `input`, and when it is locked, opens it with what `ask` gives,
/// as long as it gives something. A password is never tried on a file that
/// opens without one: qpdf refuses a wrong one even there.
pub fn open(qpdf: &Path, input: &Path, ask: &mut AskPassword) -> Result<PdfInfo> {
    match read(qpdf, input, None) {
        Err(e) if e.code == ErrorCode::PasswordProtected => {}
        other => return other,
    }
    let mut wrong = false;
    loop {
        let Some(password) = ask(input, wrong) else {
            return Err(if wrong {
                wrong_password(input)
            } else {
                password_needed(input)
            });
        };
        match read(qpdf, input, Some(&password)) {
            Err(e) if e.code == ErrorCode::PasswordProtected => wrong = true,
            other => return other,
        }
    }
}

/// Every page a bookmark leads to, its children's included.
fn collect_bookmarked(items: &serde_json::Value, into: &mut Vec<u32>) {
    for item in items.as_array().into_iter().flatten() {
        if let Some(page) = item["destpageposfrom1"].as_u64() {
            into.push(page as u32);
        }
        collect_bookmarked(&item["kids"], into);
    }
}

/// Turns one finished `qpdf --json` read into a `PdfInfo`. qpdf exits 0 on
/// success, 3 when it succeeded but had to repair the file, and 2 when it
/// could not open it at all, as when it needs a password (or another one).
pub fn parse_read(input: &Path, code: Option<i32>, stdout: &[u8], stderr: &str) -> Result<PdfInfo> {
    let name = display_name(input);
    let unreadable =
        |reason: &str| qpdf_error(format!("{name} could not be read as a PDF: {reason}"));
    match code {
        Some(0) | Some(3) => {}
        Some(2) if stderr.contains("invalid password") => return Err(password_needed(input)),
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
    let mut bookmarked_pages = Vec::new();
    collect_bookmarked(&v["outlines"], &mut bookmarked_pages);
    let enc = &v["encrypt"];
    let encrypted = enc["encrypted"].as_bool() == Some(true);
    // What whoever opened it may do. A file opened with its owner password
    // may do everything; one that allows everything restricts nothing, even
    // encrypted. Without the list, an encrypted file is taken to restrict.
    let allows_all = match enc["capabilities"].as_object() {
        Some(caps) => caps.values().all(|c| c.as_bool() != Some(false)),
        None => false,
    };
    let restricted =
        encrypted && enc["ownerpasswordmatched"].as_bool() != Some(true) && !allows_all;
    Ok(PdfInfo {
        path: input.to_path_buf(),
        pages,
        has_bookmarks,
        bookmarked_pages,
        encrypted,
        restricted,
        password: None,
        damaged: code == Some(3),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `qpdf --json` report: `caps` is the capabilities list, `None` to
    /// leave it out.
    fn report_with(
        encrypted: bool,
        owner_matched: bool,
        caps: Option<serde_json::Value>,
        outlines: serde_json::Value,
        pages: usize,
    ) -> Vec<u8> {
        let pages: Vec<_> = (0..pages)
            .map(
                |i| serde_json::json!({"object": format!("{} 0 R", i + 10), "pageposfrom1": i + 1}),
            )
            .collect();
        let mut encrypt = serde_json::json!({
            "encrypted": encrypted,
            "userpasswordmatched": true,
            "ownerpasswordmatched": owner_matched
        });
        if let Some(caps) = caps {
            encrypt["capabilities"] = caps;
        }
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "parameters": {"decodelevel": "generalized"},
            "pages": pages,
            "encrypt": encrypt,
            "outlines": outlines
        }))
        .unwrap()
    }

    fn report(encrypted: bool, owner_matched: bool, outlines: usize, pages: usize) -> Vec<u8> {
        let outlines: Vec<_> = (0..outlines)
            .map(|i| serde_json::json!({"title": format!("b{i}"), "kids": []}))
            .collect();
        report_with(encrypted, owner_matched, None, outlines.into(), pages)
    }

    fn caps(print: bool) -> serde_json::Value {
        serde_json::json!({
            "accessibility": true, "extract": true, "modify": true,
            "printhigh": print, "printlow": print
        })
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
                bookmarked_pages: vec![],
                encrypted: false,
                restricted: false,
                password: None,
                damaged: false,
            }
        );
        assert!(!info.locked());
    }

    #[test]
    fn bookmarks_restrictions_and_repairs_are_noticed() {
        let info = parse_read(Path::new("a.pdf"), Some(0), &report(true, false, 2, 1), "").unwrap();
        assert!(info.has_bookmarks);
        assert!(info.encrypted);
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
    fn an_encrypted_file_that_allows_everything_restricts_nothing() {
        let r = |print| {
            let bytes = report_with(true, false, Some(caps(print)), serde_json::json!([]), 1);
            parse_read(Path::new("a.pdf"), Some(0), &bytes, "").unwrap()
        };
        assert!(!r(true).restricted);
        assert!(r(true).encrypted);
        assert!(r(false).restricted);
    }

    #[test]
    fn bookmarked_pages_are_collected_from_every_level() {
        let outlines = serde_json::json!([
            {"title": "One", "destpageposfrom1": 1, "kids": [
                {"title": "One.a", "destpageposfrom1": 2, "kids": []},
                {"title": "Nowhere", "kids": []}
            ]},
            {"title": "Three", "destpageposfrom1": 3, "kids": []}
        ]);
        let bytes = report_with(false, false, None, outlines, 3);
        let info = parse_read(Path::new("a.pdf"), Some(0), &bytes, "").unwrap();
        assert_eq!(info.bookmarked_pages, vec![1, 2, 3]);
    }

    #[test]
    fn a_password_protected_pdf_needs_its_password() {
        let e = parse_read(
            Path::new("enc.pdf"),
            Some(2),
            b"",
            "qpdf: enc.pdf: invalid password\n",
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::PasswordProtected);
        assert_eq!(e.message, "enc.pdf is password-protected");
        assert_eq!(
            e.remediation.unwrap().manual.unwrap(),
            "run it in a terminal to be asked for the password, or set CONVKIT_PASSWORD"
        );
        assert_eq!(
            wrong_password(Path::new("dir/enc.pdf")).message,
            "wrong password for enc.pdf"
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
    fn the_read_asks_for_exactly_three_keys_after_the_password() {
        assert_eq!(
            read_args(Path::new("-x.pdf"), None, true),
            vec![
                "--json",
                "--json-key=encrypt",
                "--json-key=outlines",
                "--json-key=pages",
                "./-x.pdf"
            ]
        );
        let p = Password::new("s3cret").unwrap();
        assert_eq!(
            read_args(Path::new("x.pdf"), Some(&p), true)[0],
            "--password=s3cret"
        );
        assert_eq!(
            read_args(Path::new("x.pdf"), Some(&p), false)[0],
            "--password=<password>"
        );
    }

    /// `open` asks only when qpdf says the file needs a password, says
    /// whether the last one was wrong, and stops when `ask` gives up.
    #[cfg(unix)]
    #[test]
    fn open_asks_until_a_password_works_or_the_asker_gives_up() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("qpdf-stub");
        // Opens with `right` (read from stdin as `@-`), refuses anything else.
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             if [ \"$1\" = @- ]; then read first; fi\n\
             if [ \"$first\" = --password=right ]; then\n\
               printf '{\"pages\":[{}],\"encrypt\":{\"encrypted\":true,\"userpasswordmatched\":true}}'\n\
               exit 0\n\
             fi\n\
             echo 'qpdf: x.pdf: invalid password' >&2; exit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = Path::new("x.pdf");

        let mut asked = Vec::new();
        let mut answers = vec!["wrong", "right"].into_iter();
        let info = open(&stub, input, &mut |p: &Path, wrong| {
            asked.push((p.to_path_buf(), wrong));
            answers.next().map(|a| Password::new(a).unwrap())
        })
        .unwrap();
        assert_eq!(asked, vec![(input.into(), false), (input.into(), true)]);
        assert!(info.locked());
        assert_eq!(info.password.unwrap().expose(), "right");

        let e = open(&stub, input, &mut |_: &Path, _| None).unwrap_err();
        assert_eq!(e.message, "x.pdf is password-protected");
        let mut once = Some(Password::new("nope").unwrap());
        let e = open(&stub, input, &mut |_: &Path, _| once.take()).unwrap_err();
        assert_eq!(e.message, "wrong password for x.pdf");
    }
}
