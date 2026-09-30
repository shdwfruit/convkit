use convkit_core::recipe::ScaleStyle;
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

/// The tuning flags that apply to a pair, as flag names.
///
/// Most are scanned from the recipe's own args: the same slots
/// `plan::build_tuned` validates against, so those entries cannot drift
/// from what actually works. Some are keyed on the target instead, because
/// nothing in the recipe carries them: webm's `--resize` and `--fps`
/// (its chain is composed at run time) and `--max-size` (a policy over a
/// whole video conversion, not a slot in one recipe).
fn tuning_flags_for(from: Format, to: Format) -> Vec<&'static str> {
    let Some(recipe) = registry::lookup(from, to) else {
        return Vec::new();
    };
    let has = |wanted: fn(&Arg) -> bool| recipe.steps.iter().any(|s| s.args.iter().any(&wanted));
    let mut flags = Vec::new();
    if has(|a| matches!(a, Arg::TuneResize)) {
        push_flag(&mut flags, "--resize");
    }
    if has(|a| matches!(a, Arg::Quality(_))) {
        push_flag(&mut flags, "--quality");
    }
    if has(|a| matches!(a, Arg::TuneColors)) {
        push_flag(&mut flags, "--colors");
    }
    // Both ScaleStyle variants advertise both flags. VideoChainSpec's `fps`
    // and `scale` describe the *authored default a knob overrides*, never
    // whether the knob is accepted, so a recipe authoring neither still
    // takes both.
    if has(|a| matches!(a, Arg::VideoChain(_))) {
        push_flag(&mut flags, "--resize");
        push_flag(&mut flags, "--fps");
    }
    if has(|a| matches!(a, Arg::Crf(_))) {
        push_flag(&mut flags, "--crf");
    }
    // VIDEO_TO_WEBM's static recipe carries no `-vf` slot -- vp9 needs no
    // even-dimension workaround, so one was never authored. But virtually
    // every real invocation has a probe, and `media::transcoded_invocation`
    // composes TRANSCODE_CHAIN for a webm target unconditionally, whatever
    // the static recipe declares. Advertise what actually runs.
    if to == Format::Webm {
        push_flag(&mut flags, "--resize");
        push_flag(&mut flags, "--fps");
    }
    // --max-size is a policy over a whole video conversion rather than a
    // slot in one recipe, so it is keyed on the target, not scanned.
    if convkit_core::sized::is_video_target(to) {
        push_flag(&mut flags, "--max-size");
    }
    flags
}

/// Pushes `flag` unless it is already present. `--resize` can be earned by
/// two different slots (`Arg::TuneResize` and `Arg::VideoChain`), and a
/// recipe carrying both must not list it twice.
fn push_flag(flags: &mut Vec<&'static str>, flag: &'static str) {
    if !flags.contains(&flag) {
        flags.push(flag);
    }
}

/// The tuning flags that hold for *every* source writing into `fmt`, not
/// just the first one in the table. Different sources sharing a target can
/// carry different slots -- soffice's `docx`/`xlsx`/`pptx`/`odt`/`ods` ->
/// `pdf` carry no tuning at all, unlike the image sources sharing that same
/// target -- so anything less than every source would claim a flag works
/// no matter where you start from, when it does not.
fn common_tuning_flags(sources: &[Format], fmt: Format) -> Vec<&'static str> {
    let mut sources = sources.iter();
    let Some(&first) = sources.next() else {
        return Vec::new();
    };
    let mut common = tuning_flags_for(first, fmt);
    for &s in sources {
        let theirs = tuning_flags_for(s, fmt);
        common.retain(|f| theirs.contains(f));
    }
    common
}

/// The anchors a `from -> to` recipe's own args bake in, as `(key, value)`
/// pairs -- the same slots `tuning_flags_for` scans, but the baked-in
/// default a knob overrides rather than the knob's own flag name. A
/// default belongs to the *pair*, not to either format alone: `mkv` as a
/// target bakes `crf 20`, `mkv -> webm` bakes `crf 32`, `mkv -> gif` bakes
/// `fps 15` -- one key cannot hold three truths.
fn default_anchors_for(from: Format, to: Format) -> Vec<(&'static str, &'static str)> {
    let Some(recipe) = registry::lookup(from, to) else {
        return Vec::new();
    };
    let mut anchors = Vec::new();
    for step in recipe.steps {
        for &arg in step.args {
            match arg {
                Arg::Quality(d) => anchors.push(("quality", d)),
                Arg::Crf(d) => anchors.push(("crf", d)),
                Arg::VideoChain(spec) => {
                    if let Some(fps) = spec.fps {
                        anchors.push(("fps", fps));
                    }
                    if let ScaleStyle::CappedLanczos { default_width } = spec.scale {
                        anchors.push(("max_width", default_width));
                    }
                }
                _ => {}
            }
        }
    }
    anchors
}

/// Renders a set of `(key, value)` anchors as a JSON object. A key with no
/// anchor is simply absent from the slice, never emitted as `null`.
fn anchors_json(anchors: &[(&'static str, &'static str)]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = anchors
        .iter()
        .map(|&(k, v)| (k.to_string(), json!(v)))
        .collect();
    serde_json::Value::Object(map)
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

    // The defaults worth publishing at the top level, aggregated the same
    // way `notes` below aggregates warnings: first-seen-per-key, over every
    // recipe that writes *into* `fmt`. A default belongs to the pair that
    // bakes it in, which is why each `targets[]` row below carries its own
    // `defaults` too -- this top-level one is only the inbound summary.
    let mut top_defaults: Vec<(&'static str, &'static str)> = Vec::new();
    for &s in &sources {
        for (k, v) in default_anchors_for(s, fmt) {
            if !top_defaults.iter().any(|&(ek, _)| ek == k) {
                top_defaults.push((k, v));
            }
        }
    }

    if cli.json {
        let target_rows: Vec<serde_json::Value> = targets
            .iter()
            .map(|&t| {
                json!({
                    "to": t,
                    "backends": registry::backends_for(fmt, t),
                    "tuning": tuning_flags_for(fmt, t),
                    "defaults": anchors_json(&default_anchors_for(fmt, t)),
                    "notes": registry::lookup(fmt, t).map(|r| r.warnings.to_vec()).unwrap_or_default(),
                })
            })
            .collect();
        let envelope = json!({
            "ok": true,
            "format": fmt,
            // `Kind`'s own `Serialize` spelling (it declares
            // `rename_all = "lowercase"`), not `Debug`. Rendering the Rust
            // variant name here made the published envelope say "Image"
            // while `conv scan --json` said "image" -- two spellings of one
            // enum across two commands, from the same field name.
            "kind": fmt.kind(),
            "sources": sources,
            "targets": target_rows,
            "defaults": anchors_json(&top_defaults),
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
        let flags = common_tuning_flags(&sources, fmt);
        if !flags.is_empty() {
            println!(
                "  tuning flags when writing {}: {}",
                fmt.ext(),
                flags.join(" ")
            );
        }
    }

    // The defaults worth knowing are the ones a flag can override -- only
    // the anchors this format's own sources actually bake in, computed
    // above -- plus the fidelity policies the recipes bake in (their own
    // warning strings).
    if !top_defaults.is_empty() {
        println!();
        for &(key, flag) in &[
            ("quality", "--quality"),
            ("crf", "--crf"),
            ("fps", "--fps"),
            ("max_width", "--resize"),
        ] {
            if let Some(&(_, value)) = top_defaults.iter().find(|&&(k, _)| k == key) {
                println!("  defaults: {key} {value} (override with {flag})");
            }
        }
    }
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
