use std::path::PathBuf;

use convkit_core::{plan, registry};

/// Renders every registered pair to argv and snapshots the whole table.
/// A change here is a deliberate change to output quality — review it.
#[test]
fn every_registered_pair_renders_stable_argv() {
    let mut lines: Vec<String> = Vec::new();
    for (from, to) in registry::all_pairs() {
        let input = PathBuf::from(format!("in.{}", from.ext()));
        let output = PathBuf::from(format!("out.{}", to.ext()));
        let p = plan::build(from, to, &[input], &output, None, None)
            .unwrap_or_else(|e| panic!("{from:?}->{to:?} failed to plan: {e}"));
        for step in &p.steps {
            lines.push(format!(
                "{} -> {}: {} {}",
                from.ext(),
                to.ext(),
                step.program,
                step.argv.join(" ")
            ));
        }
    }
    lines.sort();
    insta::assert_snapshot!(lines.join("\n"));
}

#[test]
fn every_registered_pair_has_at_least_one_step() {
    for (from, to) in registry::all_pairs() {
        let r = registry::lookup(from, to).unwrap();
        assert!(!r.steps.is_empty(), "{from:?}->{to:?} has no steps");
        let last = r.steps.len() - 1;
        for (i, s) in r.steps.iter().enumerate() {
            if i == last {
                assert!(
                    s.intermediate_ext.is_none(),
                    "final step of {from:?}->{to:?} must not declare intermediate_ext"
                );
            } else {
                assert!(
                    s.intermediate_ext.is_some(),
                    "non-final step of {from:?}->{to:?} must declare intermediate_ext"
                );
            }
        }
    }
}
