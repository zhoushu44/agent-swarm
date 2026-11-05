//! Regenerate the golden fixtures after an intentional prompt change.
//!
//! ```text
//! cargo run -p swarm-prompts --example bless_golden
//! ```
//!
//! Run this ONLY when the prompt change is deliberate. The fixtures exist to
//! catch accidental drift; blessing them turns a failing test green without
//! telling you whether the new prompt is better.
use std::fs;
use std::path::Path;
use swarm_prompts::{build_worker_prompt, Options};

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");

    let cases: Vec<(&str, Options)> = vec![
        (
            "direct_full",
            Options {
                peer_name: "w3".into(),
                server_port: 17771,
                mission: "Ship the CSV exporter.".into(),
                conventions: "Use tabs. Never touch migrations/.".into(),
                turn: 4,
                brief: "LAST TURN: claimed i7\nBOARD SNAPSHOT: 2 pending / 1 in_progress / 0 done".into(),
                harness: "direct".into(),
                ..Default::default()
            },
        ),
        (
            "direct_minimal",
            Options {
                peer_name: "w1".into(),
                server_port: 17771,
                harness: "direct".into(),
                ..Default::default()
            },
        ),
        (
            "cli_harness",
            Options {
                peer_name: "w2".into(),
                server_port: 18000,
                mission: "M".into(),
                turn: 1,
                harness: "cli".into(),
                ..Default::default()
            },
        ),
        (
            "with_token",
            Options {
                peer_name: "w9".into(),
                server_port: 17771,
                token: "tok-abc".into(),
                mission: "M".into(),
                harness: "direct".into(),
                ..Default::default()
            },
        ),
    ];

    for (name, opts) in cases {
        let body = build_worker_prompt(&opts);
        let path = dir.join(format!("{name}.txt"));
        fs::write(&path, &body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        println!("blessed {} ({} bytes)", path.display(), body.len());
    }
}
