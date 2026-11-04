use std::path::PathBuf;

use convkit_core::speech::Engine;
use convkit_core::{plan, registry, AvailableBackends, Backend, Format, Tuning};

/// Renders every registered pair to argv and snapshots the whole table.
/// A change here is a deliberate change to output quality — review it.
///
/// Text read aloud is left out: the table holds the speech engine of the
/// system the test runs on, and `every_speech_engine_renders_stable_argv`
/// snapshots all four instead.
#[test]
fn every_registered_pair_renders_stable_argv() {
    let mut lines: Vec<String> = Vec::new();
    for (from, to) in registry::all_pairs() {
        if registry::speaks(from, to) {
            continue;
        }
        let input = PathBuf::from(format!("in.{}", from.ext()));
        let output = PathBuf::from(format!("out.{}", to.ext()));
        let p = plan::build(from, to, &[input], &output, None, None)
            .unwrap_or_else(|e| panic!("{from:?}->{to:?} failed to plan: {e}"));
        // A pair conv converts itself has no command to show.
        if p.built_in {
            lines.push(format!("{} -> {}: built in", from.ext(), to.ext()));
        }
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

/// Every pair that reads text aloud, with every engine, whichever system
/// runs the test: say, Windows' System.Speech, espeak-ng, and Piper (with a
/// voice, which it needs). An RTF source also renders its pandoc fallback.
#[test]
fn every_speech_engine_renders_stable_argv() {
    let mut lines: Vec<String> = Vec::new();
    let no_soffice: AvailableBackends = [Backend::Pandoc].into_iter().collect();
    for engine in Engine::all() {
        let tuning = Tuning {
            voice: (engine == Engine::Piper).then(|| "en_US-lessac-medium.onnx".to_string()),
            ..Tuning::default()
        };
        for (from, to) in registry::all_pairs() {
            if !registry::speaks(from, to) {
                continue;
            }
            let input = PathBuf::from(format!("in.{}", from.ext()));
            let output = PathBuf::from(format!("out.{}", to.ext()));
            let mut routes = vec![("", None)];
            if from == Format::Rtf {
                routes.push((" (no soffice)", Some(&no_soffice)));
            }
            for (route, available) in routes {
                let p = plan::build_speech(
                    engine,
                    from,
                    to,
                    std::slice::from_ref(&input),
                    &output,
                    None,
                    available,
                    &tuning,
                )
                .unwrap_or_else(|e| panic!("{engine:?} {from:?}->{to:?} failed to plan: {e}"));
                for step in &p.steps {
                    lines.push(format!(
                        "{engine:?}{route}: {} -> {}: {} {}",
                        from.ext(),
                        to.ext(),
                        step.program,
                        step.argv.join(" ")
                    ));
                }
            }
        }
    }
    lines.sort();
    insta::assert_snapshot!(lines.join("\n"));
}

#[test]
fn every_registered_pair_has_at_least_one_step() {
    for (from, to) in registry::all_pairs() {
        let r = registry::lookup(from, to).unwrap();
        // Data files are converted in process, and only they have none.
        if registry::is_built_in(from, to) {
            assert!(r.steps.is_empty(), "{from:?}->{to:?} is built in");
            continue;
        }
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
