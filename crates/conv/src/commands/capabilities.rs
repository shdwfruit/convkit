use convkit_core::{registry, Arg, ConvError, Format};
use serde_json::json;

use crate::cli::Cli;
use crate::render;

/// Lists every supported conversion pair — or, given a format, that
/// format's own view: pairs in and out, the defaults its recipes bake in,
/// and which tuning flags apply per target. The per-format view exists
/// because the knobs are per-*recipe*, not global: `--quality` means
/// something for `heic -> jpg` and nothing for `heic -> png`, and the only
/// honest place to learn that without trial and error is here.
pub fn run(cli: &Cli, format: Option<&str>) -> i32 {
    if let Some(ext) = format {
        return format_detail(cli, ext);
    }
    let pairs = registry::all_pairs();

    if cli.json {
        let arr: Vec<serde_json::Value> = pairs
            .iter()
            .map(|&(from, to)| {
                json!({
                    "from": from,
                    "to": to,
                    "backends": registry::backends_for(from, to),
                })
            })
            .collect();
        let envelope = json!({ "ok": true, "pairs": arr });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        let mut current_kind = None;
        for &(from, to) in &pairs {
            let kind = from.kind();
            if current_kind != Some(kind) {
                if current_kind.is_some() {
                    println!();
                }
                println!("{kind:?}:");
                current_kind = Some(kind);
            }
            let backends: Vec<&str> = registry::backends_for(from, to)
                .iter()
                .map(|b| b.exe_name())
                .collect();
            println!(
                "  {:<6} -> {:<6} {}",
                from.ext(),
                to.ext(),
                backends.join(", ")
            );
        }
    }
    0
}

/// The tuning flags whose slots a pair's recipe carries, as flag names.
/// Scanned from the recipe's own args — the same slots `plan::build_tuned`
/// validates against, so this listing can never drift from what actually
/// works.
fn tuning_flags_for(from: Format, to: Format) -> Vec<&'static str> {
    let Some(recipe) = registry::lookup(from, to) else {
        return Vec::new();
    };
    let has = |wanted: fn(&Arg) -> bool| recipe.steps.iter().any(|s| s.args.iter().any(&wanted));
    let mut flags = Vec::new();
    if has(|a| matches!(a, Arg::TuneResize)) {
        flags.push("--resize");
    }
    if has(|a| matches!(a, Arg::Quality(_))) {
        flags.push("--quality");
    }
    if has(|a| matches!(a, Arg::TuneColors)) {
        flags.push("--colors");
    }
    flags
}

/// One format's view: sources, targets, per-target tuning flags, defaults,
/// and the fidelity notes its recipes carry.
fn format_detail(cli: &Cli, ext: &str) -> i32 {
    let Some(fmt) = Format::from_ext(ext) else {
        let e = ConvError::unknown_format(ext);
        render::print_error(cli.json, &e);
        return e.code.exit_code();
    };

    let pairs = registry::all_pairs();
    let sources: Vec<Format> = pairs
        .iter()
        .filter(|&&(_, t)| t == fmt)
        .map(|&(f, _)| f)
        .collect();
    let targets: Vec<Format> = pairs
        .iter()
        .filter(|&&(f, _)| f == fmt)
        .map(|&(_, t)| t)
        .collect();

    if cli.json {
        let target_rows: Vec<serde_json::Value> = targets
            .iter()
            .map(|&t| {
                json!({
                    "to": t,
                    "backends": registry::backends_for(fmt, t),
                    "tuning": tuning_flags_for(fmt, t),
                    "notes": registry::lookup(fmt, t).map(|r| r.warnings.to_vec()).unwrap_or_default(),
                })
            })
            .collect();
        let envelope = json!({
            "ok": true,
            "format": fmt,
            "kind": format!("{:?}", fmt.kind()),
            "sources": sources,
            "targets": target_rows,
            "defaults": { "quality": registry::IMAGE_QUALITY },
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        return 0;
    }

    println!("{} ({:?})", fmt.ext(), fmt.kind());
    if targets.is_empty() {
        println!("\n  not convertible from — read-only or unsupported as a source");
    } else {
        println!("\n  as source, converts to:");
        for &t in &targets {
            let flags = tuning_flags_for(fmt, t);
            let flags = if flags.is_empty() {
                String::new()
            } else {
                format!("   [{}]", flags.join(" "))
            };
            println!("    {} -> {:<6}{}", fmt.ext(), t.ext(), flags);
        }
    }
    if sources.is_empty() {
        println!("\n  no format converts into {}", fmt.ext());
    } else {
        let list: Vec<&str> = sources.iter().map(|s| s.ext()).collect();
        println!("\n  as target, accepts: {}", list.join(" "));
        let flags = sources
            .first()
            .map(|&s| tuning_flags_for(s, fmt))
            .unwrap_or_default();
        if !flags.is_empty() {
            println!(
                "  tuning flags when writing {}: {}",
                fmt.ext(),
                flags.join(" ")
            );
        }
    }

    // The defaults worth knowing are the ones a flag can override plus the
    // fidelity policies the recipes bake in (their own warning strings).
    println!(
        "\n  defaults: quality {} (override with --quality)",
        registry::IMAGE_QUALITY
    );
    let mut notes: Vec<&str> = Vec::new();
    for &s in &sources {
        if let Some(r) = registry::lookup(s, fmt) {
            for w in r.warnings {
                if !notes.contains(w) {
                    notes.push(w);
                }
            }
        }
    }
    for n in notes {
        println!("  note: {n}");
    }
    println!(
        "\n  full pair list: conv capabilities; exact command preview: conv <in> <out> --dry-run"
    );
    0
}
