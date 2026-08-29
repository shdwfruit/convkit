use convkit_core::registry;
use serde_json::json;

use crate::cli::Cli;

/// Lists every supported conversion pair. `registry::all_pairs()` already
/// comes back ordered by `(Format, Format)`, and `Format`'s declaration
/// order groups video, then audio, then image, then document formats
/// together — so the natural order already groups by `Kind` without an
/// extra sort.
pub fn run(cli: &Cli) -> i32 {
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
