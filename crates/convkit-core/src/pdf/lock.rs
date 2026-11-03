//! Passwords: the one that opens a locked PDF, and the lock conv puts on
//! a PDF it writes (AES-256, with optional restrictions).
//!
//! A password never goes on qpdf's command line, where any user on the
//! machine could read it in the process list; see `run::spawn`. `Debug`
//! prints none of it either.

use crate::error::{ConvError, ErrorCode, Result};

/// The environment variable conv reads a password from, so a script never
/// has to put one on the command line. The same one opens and locks a zip.
pub const PASSWORD_ENV: &str = "CONVKIT_PASSWORD";

/// A password, as typed.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<hidden>)")
    }
}

impl Password {
    /// Refuses an empty password, which locks nothing, and one with a line
    /// break, which qpdf would read as two arguments.
    pub fn new(text: impl Into<String>) -> Result<Password> {
        let text = text.into();
        if text.is_empty() {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "the password is empty",
            ));
        }
        if text.contains(['\n', '\r']) {
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                "a password can't contain a line break",
            ));
        }
        Ok(Password(text))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// What a lock can forbid someone who opens the PDF with its password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Restriction {
    Print,
    Copy,
    Edit,
}

impl Restriction {
    pub fn parse(text: &str) -> std::result::Result<Restriction, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "print" | "printing" => Ok(Restriction::Print),
            "copy" | "copying" => Ok(Restriction::Copy),
            "edit" | "editing" => Ok(Restriction::Edit),
            _ => Err(format!(
                "invalid restriction '{text}'; expected print, copy or edit"
            )),
        }
    }

    /// The `-ing` word, for notes.
    fn activity(&self) -> &'static str {
        match self {
            Restriction::Print => "printing",
            Restriction::Copy => "copying",
            Restriction::Edit => "editing",
        }
    }

    /// qpdf's 256-bit option for it. Edit forbids every kind of change:
    /// pages, forms, comments and content.
    fn qpdf_arg(&self) -> &'static str {
        match self {
            Restriction::Print => "--print=none",
            Restriction::Copy => "--extract=n",
            Restriction::Edit => "--modify=none",
        }
    }
}

/// The lock on a PDF conv writes: AES-256 with `password` to open it.
///
/// A PDF has two passwords. The user password opens it; the owner password
/// also lifts its restrictions in apps that honour them. Without
/// restrictions the two are the same, so whoever has the password has the
/// whole file, as with any locked file. With restrictions the owner
/// password is random and thrown away, since one password that also lifted
/// them would restrict nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub password: Password,
    owner: Password,
    /// Sorted, without repeats.
    pub restrict: Vec<Restriction>,
}

impl Lock {
    pub fn new(password: Password, restrict: &[Restriction]) -> Result<Lock> {
        let mut restrict = restrict.to_vec();
        restrict.sort();
        restrict.dedup();
        let owner = if restrict.is_empty() {
            password.clone()
        } else {
            random_password()?
        };
        Ok(Lock {
            password,
            owner,
            restrict,
        })
    }

    /// qpdf's `--encrypt ... --` group. With `reveal` unset the passwords
    /// are shown as `<new password>` and `<random>`, for `--dry-run` and
    /// `-v`.
    pub(crate) fn args(&self, reveal: bool) -> Vec<String> {
        let shown = |p: &Password, placeholder: &str| {
            if reveal {
                p.expose().to_string()
            } else {
                placeholder.to_string()
            }
        };
        let owner = if self.restrict.is_empty() {
            "<new password>"
        } else {
            "<random>"
        };
        let mut v = vec![
            "--encrypt".to_string(),
            format!(
                "--user-password={}",
                shown(&self.password, "<new password>")
            ),
            format!("--owner-password={}", shown(&self.owner, owner)),
            "--bits=256".to_string(),
        ];
        v.extend(self.restrict.iter().map(|r| r.qpdf_arg().to_string()));
        v.push("--".to_string());
        v
    }

    /// The note a restricted lock gets: what it turns off, and that it is
    /// the app's to honour.
    pub(crate) fn note(&self, what: &str) -> Option<String> {
        if self.restrict.is_empty() {
            return None;
        }
        let words: Vec<String> = self
            .restrict
            .iter()
            .map(|r| r.activity().to_string())
            .collect();
        Some(format!(
            "{what} opens with the password but turns off {}; PDF apps honour that, \
             though nothing forces them to.",
            super::range::join_and(&words)
        ))
    }
}

/// 24 random bytes as hex: an owner password nobody is meant to know.
fn random_password() -> Result<Password> {
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).map_err(|e| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            format!("could not make a random owner password: {e}"),
        )
    })?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Password::new(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(s: &str) -> Password {
        Password::new(s).unwrap()
    }

    #[test]
    fn a_password_never_shows_in_debug_output() {
        let p = pw("hunter2");
        assert_eq!(format!("{p:?}"), "Password(<hidden>)");
        let lock = Lock::new(p, &[Restriction::Print]).unwrap();
        assert!(!format!("{lock:?}").contains("hunter2"));
    }

    #[test]
    fn an_empty_password_or_one_with_a_line_break_is_refused() {
        assert_eq!(
            Password::new("").unwrap_err().message,
            "the password is empty"
        );
        for bad in ["a\nb", "a\r\nb", "ab\n"] {
            assert_eq!(
                Password::new(bad).unwrap_err().message,
                "a password can't contain a line break"
            );
        }
        assert_eq!(pw(" spaces kept ").expose(), " spaces kept ");
    }

    #[test]
    fn restrictions_parse_by_name() {
        assert_eq!(Restriction::parse("print"), Ok(Restriction::Print));
        assert_eq!(Restriction::parse("Copying"), Ok(Restriction::Copy));
        assert_eq!(Restriction::parse("edit"), Ok(Restriction::Edit));
        assert_eq!(
            Restriction::parse("fax").unwrap_err(),
            "invalid restriction 'fax'; expected print, copy or edit"
        );
    }

    #[test]
    fn without_restrictions_the_owner_password_is_the_password() {
        let lock = Lock::new(pw("open sesame"), &[]).unwrap();
        assert_eq!(
            lock.args(true),
            vec![
                "--encrypt",
                "--user-password=open sesame",
                "--owner-password=open sesame",
                "--bits=256",
                "--"
            ]
        );
        assert_eq!(
            lock.args(false),
            vec![
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<new password>",
                "--bits=256",
                "--"
            ]
        );
        assert_eq!(lock.note("out.pdf"), None);
    }

    #[test]
    fn restrictions_get_a_random_owner_password_and_their_qpdf_options() {
        let lock = Lock::new(
            pw("pw"),
            &[Restriction::Edit, Restriction::Print, Restriction::Edit],
        )
        .unwrap();
        assert_eq!(lock.restrict, vec![Restriction::Print, Restriction::Edit]);
        let shown = lock.args(false);
        assert_eq!(
            shown,
            vec![
                "--encrypt",
                "--user-password=<new password>",
                "--owner-password=<random>",
                "--bits=256",
                "--print=none",
                "--modify=none",
                "--"
            ]
        );
        let real = lock.args(true);
        let owner = real[2].strip_prefix("--owner-password=").unwrap();
        assert_eq!(owner.len(), 48);
        assert_ne!(owner, "pw");
        let other = Lock::new(pw("pw"), &[Restriction::Print]).unwrap();
        assert_ne!(
            other.args(true)[2],
            real[2],
            "a fresh owner password each time"
        );
        assert_eq!(
            lock.note("out.pdf").unwrap(),
            "out.pdf opens with the password but turns off printing and editing; \
             PDF apps honour that, though nothing forces them to."
        );
    }
}
