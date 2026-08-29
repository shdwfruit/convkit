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

pub fn plan_json(plan: &ConversionPlan) -> serde_json::Value {
    json!({ "ok": true, "dry_run": true, "plan": plan })
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
