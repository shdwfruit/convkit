use convkit_core::{ConvError, ConversionPlan, Outcome};
use serde_json::json;

/// Shell-ish rendering for humans. Quoting is display-only — execution passes
/// argv directly and never goes through a shell.
pub fn plan_human(plan: &ConversionPlan) -> String {
    let mut s = String::new();
    for step in &plan.steps {
        s.push_str(&step.program);
        for a in &step.argv {
            s.push(' ');
            if a.contains(' ') {
                s.push_str(&format!("{a:?}"));
            } else {
                s.push_str(a);
            }
        }
        s.push('\n');
    }
    for w in &plan.warnings {
        s.push_str(&format!("note: {w}\n"));
    }
    s
}

pub fn outcome_human(o: &Outcome) -> String {
    let kb = o.bytes as f64 / 1024.0;
    let mut s = format!("{} ({kb:.0} KB)", o.output.display());
    if o.remuxed {
        s.push_str(" [stream copy]");
    }
    s.push('\n');
    for w in &o.warnings {
        s.push_str(&format!("note: {w}\n"));
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
pub fn print_error(json: bool, e: &ConvError) {
    if json {
        eprintln!("{}", serde_json::to_string_pretty(&error_json(e)).unwrap());
    } else {
        eprint!("{}", error_human(e));
    }
}

pub fn outcome_json(o: &Outcome) -> serde_json::Value {
    json!({
        "ok": true,
        "output": o.output,
        "bytes": o.bytes,
        "remuxed": o.remuxed,
        "warnings": o.warnings,
        "backends": o.backends.iter()
            .map(|(b, v)| json!({ "backend": b, "version": v }))
            .collect::<Vec<_>>(),
    })
}
