//! Asking for passwords, the way `--password` works for zips: the flag
//! takes no value. conv asks without showing what is typed, or reads
//! CONVKIT_PASSWORD, so a password never lands in the shell's history.
//!
//! Opening a locked PDF needs no flag: conv asks when qpdf says the file
//! needs a password. `--password` is only ever about the lock on what conv
//! writes.

use std::path::{Path, PathBuf};

use convkit_core::pdf::{Lock, Password, Restriction, PASSWORD_ENV};
use convkit_core::{ConvError, ErrorCode};

use crate::prompt;

/// How many times conv asks for one file's password before giving up.
const TRIES: u32 = 3;

fn invalid(message: String) -> ConvError {
    ConvError::new(ErrorCode::InvalidInvocation, message)
}

/// CONVKIT_PASSWORD, when set and not empty.
pub fn env_password() -> Option<String> {
    std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty())
}

/// Reads a password without showing it.
pub(crate) fn ask_hidden(question: &str) -> Option<String> {
    eprint!("{}", crate::render::safe_for_terminal(question));
    #[cfg(unix)]
    let typed = read_unechoed();
    #[cfg(not(unix))]
    let typed = console::Term::stderr().read_secure_line().ok();
    typed.filter(|p| !p.is_empty())
}

/// One line from the terminal, unechoed, with signals off as the clipboard
/// reads it: a Ctrl-C stopping conv in `read_secure_line` left the terminal
/// without echo, and here it is a byte that ends the question instead.
#[cfg(unix)]
fn read_unechoed() -> Option<String> {
    use crate::clipboard::terminal::{Answer, Tty};
    let mut tty = Tty::open()?;
    let answer = tty.answer(std::time::Duration::from_secs(24 * 3600), |buf| {
        let end = buf.iter().position(|&b| b == b'\r' || b == b'\n')?;
        Some(typed_line(&buf[..end]))
    });
    drop(tty);
    eprintln!();
    match answer {
        Answer::Got(line) => Some(line),
        Answer::TimedOut | Answer::Stopped => None,
    }
}

/// What raw keys typed, with each backspace taking back a character.
#[cfg(unix)]
fn typed_line(keys: &[u8]) -> String {
    let mut s = String::new();
    for c in String::from_utf8_lossy(keys).chars() {
        match c {
            '\u{7f}' | '\u{8}' => {
                s.pop();
            }
            c => s.push(c),
        }
    }
    s
}

/// `--password hunter2` reads as the flag and a path named `hunter2`. Said
/// so, rather than reported as a missing file, since the password is now
/// in the shell's history.
pub fn typed_after_the_flag(token: &str) -> ConvError {
    invalid(format!(
        "--password takes no value: conv asks for the password so it stays out of your \
         shell history. {token} is not a file; if it was the password, it is in your shell \
         history now, so pick another"
    ))
}

/// The password a new lock gets: CONVKIT_PASSWORD, or asked twice in a
/// terminal. `what` names the file in the question.
fn new_password(what: &str) -> Result<Password, ConvError> {
    if let Some(p) = env_password() {
        return Password::new(p);
    }
    if !prompt::is_interactive_session() {
        return Err(invalid(format!(
            "--password needs a terminal to ask in, or the password in {PASSWORD_ENV}"
        )));
    }
    let first = ask_hidden(&format!("Password for {what}: "))
        .ok_or_else(|| invalid("no password given; nothing was written".to_string()))?;
    let again = ask_hidden("Again, to check: ").unwrap_or_default();
    if first != again {
        return Err(invalid(
            "the two passwords differ; nothing was written".to_string(),
        ));
    }
    Password::new(first)
}

/// The lock `--password` (and `--restrict`) ask for, or `None` without
/// the flag. A dry run writes nothing, so it asks nothing: its commands
/// show the password as a placeholder anyway.
pub fn lock_for(
    password: bool,
    restrict: &[Restriction],
    what: &str,
    dry_run: bool,
) -> Result<Option<Lock>, ConvError> {
    if !password {
        return Ok(None);
    }
    let password = if dry_run {
        Password::new("<new password>")?
    } else {
        new_password(what)?
    };
    Lock::new(password, restrict).map(Some)
}

/// Asks a question on the terminal and reads the answer without showing it.
type Asker<'a> = Box<dyn FnMut(&str) -> Option<String> + 'a>;

/// Answers qpdf's "this file needs a password" for every PDF one run
/// opens. With CONVKIT_PASSWORD set, that is the only password tried, as
/// for a zip. Otherwise conv asks in a terminal, up to three times a file,
/// and tries the passwords that worked on earlier files first, so a folder
/// of statements locked with one password asks once.
pub struct Opener<'a> {
    env: Option<Password>,
    interactive: bool,
    ask: Asker<'a>,
    /// Passwords that opened a file this run, latest last.
    worked: Vec<Password>,
    /// The file being opened, how many of `worked` were tried on it, how
    /// many times it was asked for, and the last password given.
    current: Option<(PathBuf, usize, u32, Option<Password>)>,
}

impl Opener<'static> {
    /// Reads CONVKIT_PASSWORD, and asks on the terminal when there is one.
    /// A CONVKIT_PASSWORD that can't be a password is refused, rather than
    /// passed over for an error that says to set it.
    pub fn from_env() -> Result<Opener<'static>, ConvError> {
        let env = env_password().map(Password::new).transpose()?;
        Ok(Opener::new(
            env,
            prompt::is_interactive_session(),
            ask_hidden,
        ))
    }
}

impl<'a> Opener<'a> {
    pub fn new(
        env: Option<Password>,
        interactive: bool,
        ask: impl FnMut(&str) -> Option<String> + 'a,
    ) -> Opener<'a> {
        Opener {
            env,
            interactive,
            ask: Box::new(ask),
            worked: Vec::new(),
            current: None,
        }
    }

    /// One answer to `AskPassword`: the next password to try on `path`,
    /// or `None` to give up. `wrong` says the last one did not open it.
    pub fn next(&mut self, path: &Path, wrong: bool) -> Option<Password> {
        if let Some(env) = &self.env {
            return (!wrong).then(|| env.clone());
        }
        // A new file: the last one's password, if any, opened it.
        if self.current.as_ref().is_none_or(|(p, ..)| p != path) {
            if let Some((_, _, _, Some(last))) = self.current.take() {
                if !self.worked.contains(&last) {
                    self.worked.push(last);
                }
            }
            self.current = Some((path.to_path_buf(), 0, 0, None));
        }
        let (_, tried, asked, last) = self.current.as_mut().expect("set above");
        if *tried < self.worked.len() {
            let p = self.worked[self.worked.len() - 1 - *tried].clone();
            *tried += 1;
            *last = None;
            return Some(p);
        }
        if !self.interactive || *asked >= TRIES {
            // Given up on: its last password did not open it either.
            *last = None;
            return None;
        }
        let name = crate::render::file_name_of(path);
        let question = if wrong && last.is_some() {
            "Wrong password. Try again: ".to_string()
        } else {
            format!("{name} is locked. Password: ")
        };
        *asked += 1;
        let typed = (self.ask)(&question).and_then(|t| Password::new(t).ok());
        *last = typed.clone();
        typed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_backspace_takes_back_what_was_typed() {
        assert_eq!(typed_line("pa\u{7f}ass\u{8}\u{8}sé".as_bytes()), "pasé");
    }

    fn pw(s: &str) -> Password {
        Password::new(s).unwrap()
    }

    /// An opener that answers from `answers` and records each question.
    fn scripted<'a>(
        answers: &'a mut Vec<&'static str>,
        questions: &'a mut Vec<String>,
    ) -> Opener<'a> {
        Opener::new(None, true, move |q: &str| {
            questions.push(q.to_string());
            (!answers.is_empty()).then(|| answers.remove(0).to_string())
        })
    }

    #[test]
    fn the_environment_password_is_the_only_one_tried() {
        let mut o = Opener::new(Some(pw("env")), true, |_: &str| panic!("asked"));
        let a = Path::new("a.pdf");
        assert_eq!(o.next(a, false), Some(pw("env")));
        assert_eq!(o.next(a, true), None);
    }

    #[test]
    fn without_a_terminal_nothing_is_asked() {
        let mut o = Opener::new(None, false, |_: &str| panic!("asked"));
        assert_eq!(o.next(Path::new("a.pdf"), false), None);
    }

    #[test]
    fn a_wrong_password_is_asked_again_up_to_three_times() {
        let (mut answers, mut questions) = (vec!["x", "y", "z", "never"], Vec::new());
        let mut o = scripted(&mut answers, &mut questions);
        let a = Path::new("dir/a.pdf");
        assert_eq!(o.next(a, false), Some(pw("x")));
        assert_eq!(o.next(a, true), Some(pw("y")));
        assert_eq!(o.next(a, true), Some(pw("z")));
        assert_eq!(o.next(a, true), None);
        drop(o);
        assert_eq!(
            questions,
            vec![
                "a.pdf is locked. Password: ",
                "Wrong password. Try again: ",
                "Wrong password. Try again: "
            ]
        );
    }

    #[test]
    fn a_password_that_worked_is_tried_first_on_the_next_file() {
        let (mut answers, mut questions) = (vec!["one", "two"], Vec::new());
        let mut o = scripted(&mut answers, &mut questions);
        let (a, b, c) = (Path::new("a.pdf"), Path::new("b.pdf"), Path::new("c.pdf"));
        assert_eq!(o.next(a, false), Some(pw("one")));
        // b: "one" is tried without asking; it is wrong, so b is asked for.
        assert_eq!(o.next(b, false), Some(pw("one")));
        assert_eq!(o.next(b, true), Some(pw("two")));
        // c: "two", then "one", before anything is asked.
        assert_eq!(o.next(c, false), Some(pw("two")));
        assert_eq!(o.next(c, true), Some(pw("one")));
        assert_eq!(o.next(c, true), None, "the answers ran out");
        drop(o);
        assert_eq!(
            questions,
            vec![
                "a.pdf is locked. Password: ",
                "b.pdf is locked. Password: ",
                "c.pdf is locked. Password: "
            ]
        );
    }

    #[test]
    fn a_typed_password_is_called_out() {
        assert!(typed_after_the_flag("hunter2")
            .message
            .starts_with("--password takes no value"));
    }

    #[test]
    fn a_dry_run_lock_asks_nothing() {
        let lock = lock_for(true, &[Restriction::Copy], "out.pdf", true)
            .unwrap()
            .unwrap();
        assert_eq!(lock.restrict, vec![Restriction::Copy]);
        assert!(lock_for(false, &[], "out.pdf", false).unwrap().is_none());
    }
}
