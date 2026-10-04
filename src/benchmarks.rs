//! Phase 7 speed budgets: hand-rolled micro-benchmarks over the hot paths
//! (output shaping, SSE draining, repo-map builds) that fail the release
//! test run when a regression lands. Dev runs only print the timings —
//! debug numbers are meaningless — while CI enforces the budgets with
//! `cargo test --release --locked benchmarks`. Criteria-style harnesses
//! would pull a whole dependency tree in for three numbers, so the budgets
//! are plain tests (documented as an adaptation in `docs/ref-notes.md`).

use std::time::Instant;

/// Run `work` `rounds` times, keep the fastest sample, print it, and — in
/// release mode — fail when it exceeds the budget. `pub(crate)` so the TUI
/// tests can pin the compose/render path with the same gate.
pub(crate) fn budget(label: &str, limit_ms: u128, rounds: u32, mut work: impl FnMut()) {
    let mut best = u128::MAX;
    for _ in 0..rounds {
        let started = Instant::now();
        work();
        best = best.min(started.elapsed().as_millis());
    }
    println!("budget {label}: {best}ms (limit {limit_ms}ms)");
    if !cfg!(debug_assertions) {
        assert!(
            best <= limit_ms,
            "{label} took {best}ms, over its {limit_ms}ms budget"
        );
    }
}

#[test]
fn output_shaping_stays_within_budget() {
    // 20k ANSI-tangled lines shaped for a tool cell: strip, dedupe
    // consecutive repeats, then head/tail split to the byte cap.
    let mut raw = String::new();
    for index in 0..10_000 {
        raw.push_str(&format!(
            "\u{1b}[32mline {index}\u{1b}[0m with trailing noise\n"
        ));
        raw.push_str(&format!("line {index} with trailing noise\n"));
    }
    budget("shape 20k ANSI lines", 100, 5, || {
        let shaped = crate::tools::truncate(raw.clone(), 16_384);
        assert!(shaped.contains("line 0"), "shaping lost content");
    });
}

#[test]
fn sse_parsing_stays_within_budget() {
    // 1000 streamed events drained one by one from a single buffer, the
    // exact shape of a long tool-call stream.
    let mut filled: Vec<u8> = Vec::new();
    for index in 0..1_000 {
        filled.extend_from_slice(format!("data: {{\"i\":{index}}}\n\n").as_bytes());
    }
    budget("drain 1000 SSE events", 50, 5, || {
        let mut pending = filled.clone();
        let mut count = 0;
        while crate::provider::take_sse_event(&mut pending).is_some() {
            count += 1;
        }
        assert_eq!(count, 1_000, "every event must survive the drain");
    });
}

#[test]
fn repo_map_build_stays_within_budget() {
    // A cold full walk of this very tree (target/ and friends are
    // skipped), the work behind the first `Repo map:` render.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    budget("cold repo-map walk", 2_000, 2, || {
        let mut map = crate::repo_map::RepoMap::default();
        map.update(root).expect("repo map update");
        assert_ne!(map.len(), 0, "the walk must find symbols");
    });
}
