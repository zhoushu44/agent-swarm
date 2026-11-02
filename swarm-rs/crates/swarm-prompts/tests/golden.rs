//! Golden tests: the rendered prompt must match the Go implementation
//! **byte for byte**.
//!
//! The fixtures in `testdata/` were produced by the Go binary itself:
//!
//! ```text
//! go run gen_prompt_golden.go swarm-rs/crates/swarm-prompts/testdata
//! ```
//!
//! A single extra space here changes what the model sees, so this test is the
//! only thing standing between a "harmless" refactor and a behaviour change.

use swarm_prompts::{build_worker_prompt, Options};

fn fixture(name: &str) -> String {
    let path = format!("{}/testdata/{}.txt", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path, e))
}

/// Assert equality and, on failure, print the first differing byte with context
/// so a one-character drift is diagnosable instead of a 13KB wall of text.
fn assert_identical(actual: &str, expected: &str, label: &str) {
    if actual == expected {
        return;
    }
    let a = actual.as_bytes();
    let e = expected.as_bytes();
    let mut i = 0;
    while i < a.len() && i < e.len() && a[i] == e[i] {
        i += 1;
    }
    let ctx_start = i.saturating_sub(80);
    let ctx_end_a = (i + 80).min(a.len());
    let ctx_end_e = (i + 80).min(e.len());
    panic!(
        "{label}: first difference at byte {i}\n\
         actual len={} expected len={}\n\
         --- actual[{ctx_start}..{ctx_end_a}] ---\n{:?}\n\
         --- expected[{ctx_start}..{ctx_end_e}] ---\n{:?}",
        a.len(),
        e.len(),
        &actual[ctx_start..ctx_end_a],
        &expected[ctx_start..ctx_end_e],
    );
}

#[test]
fn golden_direct_full() {
    let actual = build_worker_prompt(&Options {
        peer_name: "w3".into(),
        server_port: 17771,
        mission: "Ship the CSV exporter.".into(),
        conventions: "Use tabs. Never touch migrations/.".into(),
        turn: 4,
        brief: "LAST TURN: claimed i7\nBOARD SNAPSHOT: 2 pending / 1 in_progress / 0 done"
            .into(),
        harness: "direct".into(),
        ..Default::default()
    });
    assert_identical(&actual, &fixture("direct_full"), "direct_full");
}

#[test]
fn golden_direct_minimal() {
    let actual = build_worker_prompt(&Options {
        peer_name: "w1".into(),
        server_port: 17771,
        harness: "direct".into(),
        ..Default::default()
    });
    assert_identical(&actual, &fixture("direct_minimal"), "direct_minimal");
}

#[test]
fn golden_cli_harness() {
    let actual = build_worker_prompt(&Options {
        peer_name: "w2".into(),
        server_port: 18000,
        mission: "M".into(),
        turn: 1,
        harness: "cli".into(),
        ..Default::default()
    });
    assert_identical(&actual, &fixture("cli_harness"), "cli_harness");
}

#[test]
fn golden_with_token() {
    let actual = build_worker_prompt(&Options {
        peer_name: "w9".into(),
        server_port: 17771,
        token: "tok-abc".into(),
        mission: "M".into(),
        harness: "direct".into(),
        ..Default::default()
    });
    assert_identical(&actual, &fixture("with_token"), "with_token");
}
