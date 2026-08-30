use std::io::IsTerminal;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anstyle::{AnsiColor, Style};
use convkit_core::{ConvError, ConversionPlan, Outcome};
use serde_json::json;

use crate::batch::JobResult;

/// The characters a token may contain and still be printed bare. Everything
/// outside this set gets quoted.
///
/// Deliberately conservative, and the same set on both platforms even though
/// the two shells disagree about which characters are special: `,` and `=`
/// are harmless in bash but are argument delimiters to cmd.exe, and quoting
/// a token that did not need it costs nothing while missing one produces a
/// line that does not run. Everything convkit actually emits as a flag
/// (`-c:v`, `+faststart`, `-crf`, `20`, `writer_pdf_import`) stays bare.
fn is_bare_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '\\' | ':' | '+' | '-')
}

/// One argv token, quoted so it survives being pasted into the shell this
/// binary was built for.
///
/// This used to quote only when a token contained a space, and then with
/// Rust's `{:?}` Debug formatting, which is not shell quoting at all: it
/// escapes backslashes (`C:\\Users\\Rick Xie`) and renders non-ASCII as
/// `\u{301}`, while leaving every token *without* a space completely bare.
/// The flagship `mp4 -> gif` command is one such token -- a filter graph full
/// of `;`, `(`, `)`, `[` and `]` -- so the README's opening demo parsed in
/// neither bash nor PowerShell (F76).
///
/// The rules differ by platform because the shells do:
///
/// * POSIX gets single quotes, which are fully literal; an embedded `'` is
///   closed, escaped and reopened (`'\''`), the standard idiom.
/// * Windows gets double quotes, which is the one form both cmd.exe and
///   PowerShell accept. PowerShell's single quotes would be more literal but
///   mean nothing to cmd.exe. The residue is narrow and worth naming: a path
///   containing `$` interpolates under PowerShell and one containing `%VAR%`
///   expands under cmd.exe. Neither appears in anything convkit generates,
///   only in a path a user chose.
fn shell_quote(token: &str) -> String {
    if !token.is_empty() && token.chars().all(is_bare_safe) {
        return token.to_string();
    }
    if cfg!(windows) {
        // A literal `"` cannot occur in a Windows path, so this only ever
        // fires on a token a caller constructed; `""` is how both shells
        // spell an escaped quote inside a quoted string.
        format!("\"{}\"", token.replace('"', "\"\""))
    } else {
        format!("'{}'", token.replace('\'', r"'\''"))
    }
}

/// Renders a plan as the command lines it stands for.
///
/// The quoting is display-only in the sense that execution never goes
/// through a shell -- argv is passed directly -- but it is real shell
/// quoting, not decoration: the README calls `--dry-run` "the actual
/// product demo" and shows its output as something to run, so a line that
/// cannot be pasted is a wrong answer rather than an ugly one.
pub fn plan_human(plan: &ConversionPlan) -> String {
    let mut s = String::new();
    for step in &plan.steps {
        s.push_str(&shell_quote(&step.program));
        for a in &step.argv {
            s.push(' ');
            s.push_str(&shell_quote(a));
        }
        s.push('\n');
    }
    for w in &plan.warnings {
        s.push_str(&format!("note: {w}\n"));
    }
    s
}

/// One resolved command line for `--verbose`: the program and argv exactly
/// as spawned, shell-quoted the same way `plan_human` quotes a preview so
/// the line is honest about what ran *and* pastable.
pub fn command_line_human(program: &str, argv: &[String]) -> String {
    let mut s = shell_quote(program);
    for a in argv {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

/// One step's full backend transcript for `--verbose`, indented under a
/// header naming the backend so interleaved parallel jobs stay legible.
pub fn verbose_report_human(backend_name: &str, report: &str) -> String {
    let mut s = format!("--- {backend_name} output ---\n");
    for line in report.lines() {
        s.push_str("  ");
        s.push_str(line);
        s.push('\n');
    }
    s
}

pub fn error_human(e: &ConvError) -> String {
    let mut s = format!("error: {}\n", e.message);
    if let Some(r) = &e.remediation {
        if let Some(m) = &r.managed {
            s.push_str(&format!("  try: {m}\n"));
        }
        if let Some(m) = &r.manual {
            s.push_str(&format!("  or:  {m}\n"));
        }
    }
    s
}

/// The `{"ok": false, "error": ...}` envelope every top-level (pre-job)
/// `--json` failure uses. Factored out (I2) so `commands/convert.rs` and
/// `commands/install.rs` share one definition of this shape instead of each
/// hand-rolling the identical `json!({ "ok": false, "error": e })`.
pub fn error_json(e: &ConvError) -> serde_json::Value {
    json!({ "ok": false, "error": e })
}

/// Reports a top-level failure — one that happened before any job could
/// even be attempted (a malformed invocation, a backend genuinely missing,
/// an install refusal) — to stderr, in whichever of human or `--json` shape
/// `json` selects. Shared by every command that can fail this way, so the
/// envelope shape can't drift between them (I2).
///
/// This is deliberately the *old*, unstyled `error_human` shape, not Part
/// 2's redesigned conversion-result rendering below: this function reports
/// on invocations that never became a job at all (bad arguments, `conv
/// install` refusing), which Part 2's brief never asked to change — only
/// "a failing conversion" (`conversion_failure_human`) did.
pub fn print_error(json: bool, e: &ConvError) {
    if json {
        eprintln!("{}", serde_json::to_string_pretty(&error_json(e)).unwrap());
    } else {
        eprint!("{}", error_human(e));
    }
}

/// `--json`'s per-job success shape. Unchanged except for the additive
/// `elapsed_ms` key (Part 2): the envelope was "recently unified to always
/// emit `ok` plus a plural key," and this must go on being exactly that
/// contract plus one new, ignorable field — never a reshaped one.
pub fn outcome_json(o: &Outcome) -> serde_json::Value {
    json!({
        "ok": true,
        "output": o.output,
        "bytes": o.bytes,
        "remuxed": o.remuxed,
        "warnings": o.warnings,
        "notes": o.notes,
        "backend_output": o.backend_output,
        "elapsed_ms": o.elapsed_ms,
        "backends": o.backends.iter()
            .map(|(b, v)| json!({ "backend": b, "version": v }))
            .collect::<Vec<_>>(),
    })
}

/// Backend-reported degradation on a *successful* conversion, rendered for
/// stderr — a script watching only stderr must see trouble even when the
/// exit code is 0. `label` names the input in batch mode (where per-job
/// success lines are suppressed) and is empty for a single job.
pub fn conversion_notes_human(label: &str, o: &Outcome, styled: bool) -> String {
    let mut s = String::new();
    for n in &o.notes {
        let line = if label.is_empty() {
            format!("warning  {n}")
        } else {
            format!("warning  {label}: {n}")
        };
        s.push_str(&paint(&line, yellow_bold(), styled));
        s.push('\n');
    }
    s
}

// --- Part 2: informative human-mode rendering for a real conversion -------
//
// Everything below is new for Part 2 and used only by `commands/convert.
// rs`'s `print_results` for a real (non-`--dry-run`) run — never by
// `--json` (unaffected by design) and never by the other human-mode
// renderers above (`plan_human`, `error_human`), which report on
// `--dry-run` previews and pre-job failures the brief never asked to
// change.

/// Whether ANSI escape sequences will actually be *interpreted* by the
/// console rather than printed as literal bytes — and, on Windows, the call
/// that makes them so.
///
/// Being a terminal is not the same as understanding ANSI. Classic conhost —
/// still the default host for cmd.exe and Windows PowerShell 5.1 on Windows
/// 10, where `HKCU\Console\VirtualTerminalLevel` is unset — ships with
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` off. Gating styling on
/// `is_terminal()` alone (which is what this module used to do) therefore
/// printed `←[1m←[32m✓←[0m` literally, and left a stack of half-drawn
/// indicatif spinner lines above it, on every single run (F163).
///
/// `anstyle_query::windows::enable_ansi_colors` both turns VT on and reports
/// whether that worked, returning `None` off Windows, where ANSI needs no
/// enabling. It is already in the dependency graph via clap/anstream — which
/// is exactly why `conv --help` always rendered correctly on conhost while
/// conv's own output did not. Turning VT on also repairs indicatif's
/// progress rendering, since that draws through the same console.
///
/// Cached, because the enabling is a process-wide side effect that should
/// happen once; `main` calls this before any output so the console is in its
/// final mode before the first byte is written.
///
/// One deliberate conservatism: the underlying call enables VT on stdout and
/// stderr *together* and reports failure if either handle refuses, so
/// redirecting one stream on Windows also drops styling from the other. That
/// trades a little colour for the guarantee that we never emit an escape the
/// console would show literally — the failure this exists to prevent.
pub fn ansi_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| anstyle_query::windows::enable_ansi_colors().unwrap_or(true))
}

/// The styling rule itself, split out from the two stream accessors below so
/// it can be tested without a console: styling requires *both* a real
/// terminal and a console that will interpret what we send it. Neither
/// condition implies the other — a redirected stream is a console that is
/// not a terminal, and conhost is a terminal that does not read ANSI.
fn styled(is_terminal: bool, ansi: bool) -> bool {
    is_terminal && ansi
}

/// Whether stdout gets colour and Unicode (`✓`/`✗`/`·`), gating
/// `conversion_success_human` and `batch_summary_human`.
pub fn stdout_styled() -> bool {
    styled(std::io::stdout().is_terminal(), ansi_supported())
}

/// The stderr counterpart, gating `conversion_failure_human` — failures are
/// reported on stderr, so that's the stream whose terminal-ness decides
/// whether its `✗` and colour appear.
pub fn stderr_styled() -> bool {
    styled(std::io::stderr().is_terminal(), ansi_supported())
}

fn green_bold() -> Style {
    AnsiColor::Green.on_default().bold()
}

fn red_bold() -> Style {
    AnsiColor::Red.on_default().bold()
}

fn yellow_bold() -> Style {
    AnsiColor::Yellow.on_default().bold()
}

fn dim() -> Style {
    Style::new().dimmed()
}

/// Wraps `text` in `style`'s ANSI codes, or leaves it bare — the one place
/// every styled fragment below funnels through, so "not a real terminal"
/// reliably means plain ASCII with no escape codes anywhere, not just in
/// the places a reviewer happened to check.
fn paint(text: &str, style: Style, enabled: bool) -> String {
    if enabled {
        format!("{}{text}{}", style.render(), style.render_reset())
    } else {
        text.to_string()
    }
}

/// A file size in the same binary-but-labelled-decimal convention common
/// file managers use (1024 B = "1 KB", not 1000) — kilobytes with no
/// decimal place, megabytes and up with one. Chosen to match Part 2's own
/// worked example exactly: 2161 KiB (the pre-Part-2 rendering) is 2.11 MiB,
/// which this formula rounds to "2.1 MB" — the exact figure the brief's
/// sample output shows for the same file.
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.0} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// Elapsed time as "0.9s"/"8.3s" below a minute, "1m05s" at or above one —
/// this is the other half of "did it hang?" Part 2 exists to answer, so it
/// stays legible at both ends: sub-second precision for a quick conversion,
/// and a duration that doesn't turn into an unreadable "127.4s" for a long
/// batch.
fn human_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = elapsed.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

/// The absolute, resolved path `conversion_success_human` and
/// `batch_summary_human` always print on their own line — Part 2's single
/// biggest fix, per the owner's own complaint ("hard to tell ... where the
/// file landed"). `std::path::absolute` never touches the filesystem (it
/// can't fail on a path that doesn't resolve to anything real), so this
/// never has a failure mode worth handling beyond falling back to the path
/// exactly as given.
fn absolute_display(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

/// Renders one job's success for a real (non-`--dry-run`) conversion:
///
/// ```text
/// ✓ long.gif · 2.1 MB · 0.9s
///   C:\Users\Rick Xie\Videos\long.gif
///   note  long inputs buffer entirely in memory for palette generation
/// ```
///
/// A lossless remux gets a fourth field on the summary line naming the good
/// outcome (`o.remuxed`) rather than staying silent about it. Warnings are
/// demoted below the path, one per line, prefixed `note` (not `note:` —
/// deliberately not colon-shouted; the demotion is the placement plus, when
/// `styled`, a dim colour, not punctuation) — never given the same visual
/// weight as the result line itself, which was one of the three problems
/// the owner's feedback named. Capped at four lines total (result, path, at
/// most two notes would already be five — in practice every recipe in this
/// registry emits at most one warning, so this bound holds in fact as well
/// as in intent).
pub fn conversion_success_human(o: &Outcome, styled: bool) -> String {
    let name = o
        .output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| o.output.display().to_string());
    let sep = if styled { " \u{b7} " } else { " - " };

    let mut fields = vec![
        name,
        human_size(o.bytes),
        human_elapsed(Duration::from_millis(o.elapsed_ms)),
    ];
    if o.remuxed {
        fields.push("stream copy, no re-encode".to_string());
    }

    let mut s = String::new();
    let glyph = if styled {
        paint("\u{2713}", green_bold(), true)
    } else {
        "OK".to_string()
    };
    s.push_str(&glyph);
    s.push(' ');
    s.push_str(&fields.join(sep));
    s.push('\n');

    s.push_str(&format!("  {}\n", absolute_display(&o.output)));

    for w in &o.warnings {
        let note = format!("note  {w}");
        s.push_str("  ");
        s.push_str(&paint(&note, dim(), styled));
        s.push('\n');
    }
    s
}

/// Renders one job's failure for a real (non-`--dry-run`) conversion:
///
/// ```text
/// ✗ report.docx → pdf
///   soffice not found
///   try  winget install TheDocumentFoundation.LibreOffice
/// ```
///
/// Shows at most one remediation line — `remediation.managed` (the fix this
/// binary can run itself, e.g. `conv install ffmpeg`) when offered, else
/// `remediation.manual` — never both: two remediation lines back to back
/// reads like a checklist the user has to pick from, and one honest "try
/// this" is more actionable. `input`'s file name (not its full path — the
/// arrow line is a short header, not the detail) pairs with `to_ext`, the
/// target format alone, mirroring how a person would describe the
/// conversion they asked for out loud.
pub fn conversion_failure_human(input: &Path, to_ext: &str, e: &ConvError, styled: bool) -> String {
    let name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| input.display().to_string());
    let arrow = if styled { "\u{2192}" } else { "->" };

    let mut s = String::new();
    let glyph = if styled {
        paint("\u{2717}", red_bold(), true)
    } else {
        "FAIL".to_string()
    };
    s.push_str(&glyph);
    s.push_str(&format!(" {name} {arrow} {to_ext}\n"));
    s.push_str(&format!("  {}\n", e.message));

    if let Some(r) = &e.remediation {
        let hint = r.managed.as_deref().or(r.manual.as_deref());
        if let Some(hint) = hint {
            let line = format!("try  {hint}");
            s.push_str("  ");
            s.push_str(&paint(&line, dim(), styled));
            s.push('\n');
        }
    }
    s
}

/// Renders a whole batch's outcome as one summary line plus one location
/// line, replacing the old per-job wall of success lines the owner singled
/// out as noise:
///
/// ```text
/// ✓ 12 converted · 1 skipped · 0 failed · 8.3s
///   C:\Users\Rick Xie\Photos\out
/// ```
///
/// `skipped` counts an `OutputExists` refusal separately from a genuine
/// `failed` — a file already sitting at the destination (the common case on
/// a repeat run without `-y`) reads as "nothing to do here," not "this
/// conversion broke," even though both are `Err` under the hood. This is a
/// rendering choice only: `batch::exit_code` and the `--json` shape both
/// still treat every `Err` identically, so a script's contract is
/// unaffected. Per-job failure lines (`conversion_failure_human`) are
/// printed separately by the caller, on stderr, for every `Err` regardless
/// of this categorisation — the summary is what changed, not what's
/// reported.
pub fn batch_summary_human(results: &[JobResult], elapsed: Duration, styled: bool) -> String {
    let mut converted = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut sample_output: Option<&Path> = None;
    for r in results {
        match &r.result {
            Ok(o) => {
                converted += 1;
                sample_output.get_or_insert(o.output.as_path());
            }
            Err(e) if e.code == convkit_core::ErrorCode::OutputExists => skipped += 1,
            Err(_) => failed += 1,
        }
    }

    let sep = if styled { " \u{b7} " } else { " - " };
    let fields = [
        format!("{converted} converted"),
        format!("{skipped} skipped"),
        format!("{failed} failed"),
        human_elapsed(elapsed),
    ];

    let glyph = if failed == 0 {
        if styled {
            paint("\u{2713}", green_bold(), true)
        } else {
            "OK".to_string()
        }
    } else if styled {
        paint("\u{2717}", red_bold(), true)
    } else {
        "FAIL".to_string()
    };

    let mut s = String::new();
    s.push_str(&glyph);
    s.push(' ');
    s.push_str(&fields.join(sep));
    s.push('\n');

    // Every successful job shares one destination in the overwhelmingly
    // common case (a single `-o`/`--outdir`, or in-place beside inputs that
    // all live in one directory) — showing one representative location is
    // therefore almost always showing *the* location, and cheaper than
    // computing a common-ancestor path for the rare case it isn't.
    if let Some(out) = sample_output
        .and_then(|p| p.parent())
        .filter(|p| !p.as_os_str().is_empty())
    {
        s.push_str(&format!("  {}\n", absolute_display(out)));
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use convkit_core::{Backend, ErrorCode, Format, Remediation};
    use std::path::PathBuf;

    fn sample_outcome(bytes: u64, remuxed: bool, warnings: Vec<&str>) -> Outcome {
        Outcome {
            output: PathBuf::from("long.gif"),
            bytes,
            warnings: warnings.into_iter().map(str::to_string).collect(),
            notes: vec![],
            backend_output: vec![],
            backends: vec![],
            remuxed,
            elapsed_ms: 900,
        }
    }

    const NO_ESCAPE: char = '\u{1b}';

    fn has_ansi(s: &str) -> bool {
        s.contains(NO_ESCAPE)
    }

    fn only_ascii(s: &str) -> bool {
        s.is_ascii()
    }

    // --- shell quoting for --dry-run (F76) -----------------------------------

    /// The exact token that made the README's opening demo unrunnable: the
    /// gif filter graph, which contains `;` (a command separator in every
    /// shell), `(`, `)`, `[` and `]`, and no spaces at all -- so the old
    /// "quote only if it contains a space" rule emitted it bare.
    #[test]
    fn the_gif_filter_graph_is_quoted_even_though_it_has_no_spaces() {
        let graph = "fps=15,scale=w=min(640\\,iw):h=-2:flags=lanczos,split[a][b];\
                     [a]palettegen=stats_mode=diff[p]";
        let quoted = shell_quote(graph);
        assert_ne!(quoted, graph, "a bare filter graph does not parse anywhere");
        assert!(quoted.starts_with(if cfg!(windows) { '"' } else { '\'' }));
        assert!(quoted.ends_with(if cfg!(windows) { '"' } else { '\'' }));
        assert!(quoted.contains("palettegen"), "{quoted}");
    }

    /// Flags and plain names stay bare, so the common line reads the way it
    /// always did rather than turning into a wall of quotes.
    #[test]
    fn ordinary_flags_and_names_are_left_bare() {
        for token in ["-i", "-y", "-crf", "20", "-c:v", "+faststart", "clip.mp4"] {
            assert_eq!(shell_quote(token), token);
        }
    }

    /// A path with a space used to be rendered with Rust's Debug formatting,
    /// which doubles every backslash -- so the printed Windows path was not
    /// the path.
    #[test]
    fn a_path_with_a_space_keeps_its_real_backslashes() {
        let path = "C:\\Users\\Rick Xie\\out dir\\clip.mp3";
        let quoted = shell_quote(path);
        assert!(!quoted.contains("\\\\"), "Debug escaping is back: {quoted}");
        assert!(quoted.contains("Rick Xie"), "{quoted}");
    }

    /// Debug formatting also escaped combining marks as `\u{301}`, so an NFD
    /// file name -- what macOS writes -- printed as something that is not the
    /// file name.
    #[test]
    fn a_combining_mark_survives_as_itself() {
        let nfd = "cafe\u{301} photo.jpg";
        assert!(shell_quote(nfd).contains('\u{301}'));
        assert!(!shell_quote(nfd).contains("\\u{"));
    }

    #[test]
    fn an_embedded_quote_is_escaped_the_way_the_host_shell_spells_it() {
        if cfg!(windows) {
            assert_eq!(shell_quote("a\"b"), "\"a\"\"b\"");
        } else {
            assert_eq!(shell_quote("it's"), r"'it'\''s'");
        }
    }

    #[test]
    fn an_empty_token_is_quoted_rather_than_vanishing() {
        let quoted = shell_quote("");
        assert!(quoted.len() == 2, "{quoted}");
    }

    // --- styling gate (F163) --------------------------------------------------

    /// Being a terminal is not enough on Windows: conhost is a real terminal
    /// that prints ANSI escapes literally until virtual-terminal processing
    /// is enabled. Both conditions are required, and neither alone suffices —
    /// this is the whole content of the F163 fix, stated as a truth table so
    /// it can be checked without a console.
    #[test]
    fn styling_needs_both_a_terminal_and_a_console_that_reads_ansi() {
        assert!(styled(true, true));
        // A real terminal that shows escapes literally (conhost, VT off).
        assert!(!styled(true, false));
        // Piped or redirected: no styling regardless of console capability.
        assert!(!styled(false, true));
        assert!(!styled(false, false));
    }

    /// `ansi_supported` is a cached, process-wide side effect (it *enables*
    /// VT on Windows), so it must answer the same way every time it is asked
    /// — `stdout_styled` and `stderr_styled` each call it, and a batch calls
    /// it again to decide whether to draw a progress bar.
    #[test]
    fn ansi_support_is_decided_once_and_stays_decided() {
        assert_eq!(ansi_supported(), ansi_supported());
    }

    /// Off Windows there is nothing to enable, so the answer is
    /// unconditionally yes; the `unwrap_or(true)` in `ansi_supported` is what
    /// makes the Windows-only call a no-op everywhere else.
    #[test]
    #[cfg(not(windows))]
    fn ansi_is_always_supported_off_windows() {
        assert!(ansi_supported());
    }

    // --- human_size / human_elapsed -----------------------------------------

    /// The exact worked example from the brief: 2161 KiB (the pre-Part-2
    /// rendering's own number for this file) must come out "2.1 MB", not
    /// "2.2 MB" — proving this uses binary (1024-based) units labelled `MB`,
    /// not decimal (1000-based) ones.
    #[test]
    fn human_size_matches_the_briefs_worked_example() {
        let bytes = 2161 * 1024;
        assert_eq!(human_size(bytes), "2.1 MB");
    }

    #[test]
    fn human_size_uses_whole_kb_below_one_mb() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(2048), "2 KB");
    }

    #[test]
    fn human_elapsed_shows_one_decimal_below_a_minute() {
        assert_eq!(human_elapsed(Duration::from_millis(900)), "0.9s");
        assert_eq!(human_elapsed(Duration::from_millis(8300)), "8.3s");
    }

    #[test]
    fn human_elapsed_switches_to_minutes_and_seconds_at_a_minute() {
        assert_eq!(human_elapsed(Duration::from_secs(65)), "1m05s");
    }

    // --- conversion_success_human --------------------------------------------

    /// Styled (real-terminal) output must carry the `✓` glyph, ANSI escapes,
    /// and — Part 2's single biggest fix — the absolute output path on its
    /// own line.
    #[test]
    fn success_styled_has_the_check_glyph_ansi_and_the_absolute_path() {
        let o = sample_outcome(2161 * 1024, false, vec![]);
        let s = conversion_success_human(&o, true);
        // The glyph is wrapped in ANSI codes when styled, so it doesn't
        // literally lead the string — it must still be present, right after
        // the opening escape sequence.
        assert!(s.contains('\u{2713}'), "{s:?}");
        assert!(s.starts_with(NO_ESCAPE), "{s:?}");
        assert!(has_ansi(&s), "{s:?}");
        let abs = std::path::absolute("long.gif").unwrap();
        assert!(s.contains(&abs.display().to_string()), "{s:?}");
        assert!(s.contains("2.1 MB"), "{s:?}");
        assert!(s.contains("0.9s"), "{s:?}");
    }

    /// Unstyled (piped/redirected) output must degrade to plain ASCII with
    /// no escape codes at all — the acceptance check's "piped through cat"
    /// case — while still carrying the same absolute path and size/elapsed
    /// figures.
    #[test]
    fn success_unstyled_is_plain_ascii_with_no_escape_codes() {
        let o = sample_outcome(2161 * 1024, false, vec![]);
        let s = conversion_success_human(&o, false);
        assert!(!has_ansi(&s), "{s:?}");
        assert!(only_ascii(&s), "{s:?}");
        assert!(s.starts_with("OK "), "{s:?}");
        let abs = std::path::absolute("long.gif").unwrap();
        assert!(s.contains(&abs.display().to_string()), "{s:?}");
    }

    #[test]
    fn success_names_a_lossless_remux_on_the_result_line() {
        let o = sample_outcome(4_200_000, true, vec![]);
        let styled = conversion_success_human(&o, true);
        let unstyled = conversion_success_human(&o, false);
        assert!(styled.contains("stream copy, no re-encode"), "{styled:?}");
        assert!(
            unstyled.contains("stream copy, no re-encode"),
            "{unstyled:?}"
        );
    }

    /// Warnings are demoted onto their own `note` line, below the path —
    /// never colon-shouted, and (styled only) dim rather than the same
    /// weight as the result.
    #[test]
    fn success_demotes_a_warning_onto_its_own_note_line() {
        let o = sample_outcome(1024, false, vec!["long inputs buffer entirely in memory"]);
        let s = conversion_success_human(&o, false);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[2].contains("note"), "{lines:?}");
        assert!(
            lines[2].contains("long inputs buffer entirely in memory"),
            "{lines:?}"
        );
    }

    #[test]
    fn success_output_is_never_more_than_four_lines() {
        let o = sample_outcome(1024, true, vec!["one warning"]);
        let s = conversion_success_human(&o, true);
        assert!(s.lines().count() <= 4, "{s:?}");
    }

    // --- conversion_failure_human ---------------------------------------------

    fn backend_missing_error(backend: Backend) -> ConvError {
        ConvError {
            code: ErrorCode::BackendMissing,
            message: format!("{} not found", backend.exe_name()),
            backend: Some(backend),
            remediation: Some(Remediation {
                managed: None,
                manual: Some("winget install TheDocumentFoundation.LibreOffice".to_string()),
            }),
        }
    }

    #[test]
    fn failure_styled_has_the_cross_glyph_and_arrow() {
        let e = backend_missing_error(Backend::Soffice);
        let s = conversion_failure_human(Path::new("report.docx"), "pdf", &e, true);
        // As above: the glyph is wrapped in ANSI codes when styled, so it
        // doesn't literally lead the string.
        assert!(s.contains('\u{2717}'), "{s:?}");
        assert!(s.starts_with(NO_ESCAPE), "{s:?}");
        assert!(s.contains("report.docx"), "{s:?}");
        assert!(s.contains('\u{2192}'), "{s:?}");
        assert!(s.contains("pdf"), "{s:?}");
        assert!(s.contains("soffice not found"), "{s:?}");
        assert!(
            s.contains("winget install TheDocumentFoundation.LibreOffice"),
            "{s:?}"
        );
    }

    #[test]
    fn failure_unstyled_is_plain_ascii_using_an_ascii_arrow() {
        let e = backend_missing_error(Backend::Soffice);
        let s = conversion_failure_human(Path::new("report.docx"), "pdf", &e, false);
        assert!(!has_ansi(&s), "{s:?}");
        assert!(only_ascii(&s), "{s:?}");
        assert!(s.starts_with("FAIL "), "{s:?}");
        assert!(s.contains("->"), "{s:?}");
        assert!(!s.contains('\u{2192}'), "{s:?}");
    }

    /// Exactly one remediation line, preferring the managed hint (the fix
    /// this binary can run itself) over the manual one when both exist —
    /// never both back to back.
    #[test]
    fn failure_prefers_the_managed_remediation_over_the_manual_one() {
        let e = ConvError {
            code: ErrorCode::BackendMissing,
            message: "ffmpeg not found".to_string(),
            backend: Some(Backend::Ffmpeg),
            remediation: Some(Remediation {
                managed: Some("conv install ffmpeg".to_string()),
                manual: Some("winget install Gyan.FFmpeg".to_string()),
            }),
        };
        let s = conversion_failure_human(Path::new("in.mov"), "mp4", &e, false);
        assert_eq!(s.matches("try").count(), 1, "{s:?}");
        assert!(s.contains("conv install ffmpeg"), "{s:?}");
        assert!(!s.contains("winget install Gyan.FFmpeg"), "{s:?}");
    }

    // --- batch_summary_human ---------------------------------------------------

    fn ok_result(output: &str) -> JobResult {
        // `batch_summary_human` reads the output path off the `Outcome`
        // itself (`Ok(o) => ... o.output`, matching real usage: `exec::run`
        // always sets `Outcome::output` from the same job the `JobResult`
        // wraps) — not off `JobResult::output` — so this must set both to
        // the same path, not delegate to `sample_outcome`'s hardcoded
        // "long.gif".
        let mut outcome = sample_outcome(1024, false, vec![]);
        outcome.output = PathBuf::from(output);
        JobResult {
            input: PathBuf::from("in"),
            output: PathBuf::from(output),
            to: Format::Jpg,
            result: Ok(outcome),
        }
    }

    fn output_exists_result() -> JobResult {
        JobResult {
            input: PathBuf::from("in"),
            output: PathBuf::from("skipped.jpg"),
            to: Format::Jpg,
            result: Err(ConvError::new(ErrorCode::OutputExists, "exists")),
        }
    }

    fn failed_result() -> JobResult {
        JobResult {
            input: PathBuf::from("in"),
            output: PathBuf::from("failed.jpg"),
            to: Format::Jpg,
            result: Err(ConvError::new(ErrorCode::ConversionFailed, "broke")),
        }
    }

    /// The brief's own worked example, boiled down to the counting rule: an
    /// `OutputExists` failure is a "skipped" file, not a "failed" one — this
    /// is a rendering choice only, `batch::exit_code` still treats every
    /// `Err` identically.
    #[test]
    fn batch_summary_counts_output_exists_as_skipped_not_failed() {
        let results = vec![
            ok_result("a.jpg"),
            ok_result("b.jpg"),
            output_exists_result(),
        ];
        let s = batch_summary_human(&results, Duration::from_millis(8300), false);
        assert!(s.contains("2 converted"), "{s:?}");
        assert!(s.contains("1 skipped"), "{s:?}");
        assert!(s.contains("0 failed"), "{s:?}");
        assert!(s.contains("8.3s"), "{s:?}");
    }

    #[test]
    fn batch_summary_counts_a_genuine_conversion_failure_as_failed() {
        let results = vec![ok_result("a.jpg"), failed_result()];
        let s = batch_summary_human(&results, Duration::from_millis(100), false);
        assert!(s.contains("1 converted"), "{s:?}");
        assert!(s.contains("0 skipped"), "{s:?}");
        assert!(s.contains("1 failed"), "{s:?}");
    }

    #[test]
    fn batch_summary_shows_a_representative_output_location() {
        let results = vec![ok_result("out/a.jpg")];
        let s = batch_summary_human(&results, Duration::from_millis(1), false);
        let abs = std::path::absolute("out").unwrap();
        assert!(s.contains(&abs.display().to_string()), "{s:?}");
    }

    #[test]
    fn batch_summary_unstyled_has_no_escape_codes() {
        let results = vec![ok_result("a.jpg"), failed_result()];
        let s = batch_summary_human(&results, Duration::from_millis(1), false);
        assert!(!has_ansi(&s), "{s:?}");
    }
}
