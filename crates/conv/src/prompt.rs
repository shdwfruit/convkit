//! Yes/no questions on the terminal, shared by the install offer and the
//! "convert anyway?" confirmation (an extreme `--max-size` target or a
//! large `--upscale`) so they can never disagree about when a session is
//! interactive.

use std::io::{IsTerminal, Write};

/// Whether this process can ask at all: stdin (where the answer comes from)
/// and stderr (where the question is printed) must both be terminals, so a
/// script, a CI runner or `conv ... < /dev/null` is never left waiting for
/// an answer that will not come.
pub fn is_interactive_session() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Prints `message` to stderr and reads one line. Only `y` or `yes` (any
/// case) is yes; EOF or a read error is no, the same as an unanswered
/// prompt.
pub fn ask(message: &str) -> bool {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{message}");
    let _ = stderr.flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Proceed,
    Ask,
    Refuse,
}

/// Who answers "convert anyway?" for an extreme `--max-size` conversion or
/// a large `--upscale`.
/// `--yes` answers it; otherwise only a real terminal outside the machine
/// output modes is asked, and everyone else is refused, never guessed for.
pub fn extreme_gate(yes: bool, json: bool, quiet: bool, interactive: bool) -> Gate {
    if yes {
        Gate::Proceed
    } else if json || quiet || !interactive {
        Gate::Refuse
    } else {
        Gate::Ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design §6.1's table, row by row.
    #[test]
    fn who_answers_the_extreme_question() {
        assert_eq!(extreme_gate(true, false, false, false), Gate::Proceed);
        assert_eq!(extreme_gate(true, true, true, false), Gate::Proceed);
        assert_eq!(extreme_gate(false, false, false, true), Gate::Ask);
        assert_eq!(extreme_gate(false, false, false, false), Gate::Refuse);
        assert_eq!(extreme_gate(false, true, false, true), Gate::Refuse);
        assert_eq!(extreme_gate(false, false, true, true), Gate::Refuse);
    }
}
