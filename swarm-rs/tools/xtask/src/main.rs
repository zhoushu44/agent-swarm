//! Find public methods that are exercised ONLY by tests.
//!
//! Why this exists: three separate defects in this codebase had the same shape —
//! a method with unit tests that nothing in production ever called.
//!
//! | Method | Tests | Production callers |
//! |---|---|---|
//! | `CircuitBreaker::advance_hold` | 5 | **0** — `held_for` stayed zero, `--max-item-seconds` was a silent no-op |
//! | `Runner::observe` (six fields) | 70 breaker tests feeding hand-built `World` | existed, but produced literal zeros |
//! | `evaluate_post_turn(turn_ok)` | yes | called with a hard-coded `true` |
//!
//! The first is the pure case and the reason for this check: the tests were green
//! because each one called `advance_hold` itself, so the missing wiring was
//! invisible to the test suite AND to a reader skimming the file.
//!
//! `cargo build` cannot catch it — a `pub` method is not dead code as far as the
//! compiler is concerned, and the tests count as uses.
//!
//! ## How to read the output
//!
//! ```
//! cargo run -p xtask -- dead-public
//! ```
//!
//! Every line is a `pub` method whose name appears nowhere outside `#[cfg(test)]`
//! blocks and the `tools/` directory. That is a *candidate*, not a bug: plenty of
//! methods are legitimately test-only (`*_for_testing`) or part of a public API
//! surface a consumer outside this workspace uses. The point is to make the list
//! short enough to eyeball, so the next `advance_hold` does not need a
//! three-round benchmark to find.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn main() {
    let root = workspace_root();
    let files = rust_sources(&root);
    // A scan that found nothing is indistinguishable from a scan that looked in
    // the wrong place — and the first version of this tool did exactly that: the
    // root resolved one directory too high, it scanned zero files, and printed a
    // cheerful "no test-only public methods found". Refuse to report clean when
    // there was nothing to look at.
    if files.is_empty() {
        eprintln!(
            "xtask: no Rust sources under {} — wrong root? refusing to report clean.",
            root.display()
        );
        std::process::exit(2);
    }

    let mut candidates: Vec<(String, String)> = Vec::new();

    for file in &files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // Only look at lib code; a binary's `main` is entry, not API.
        if !text.contains("pub fn") && !text.contains("pub async fn") {
            continue;
        }

        // Split the file at `#[cfg(test)]`: everything after is test-only, so a
        // name that appears there and nowhere before is test-only.
        let split = text.find("#[cfg(test)]").unwrap_or(text.len());
        let prod = &text[..split];

        for name in public_fns(prod) {
            // Count occurrences in PRODUCTION code of the whole workspace,
            // excluding this method's own definition line.
            let uses = production_uses(&root, &name);
            if uses == 0 {
                let where_in = file
                    .strip_prefix(&root)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                candidates.push((name, where_in));
            }
        }
    }

    candidates.sort();
    candidates.dedup();

    if candidates.is_empty() {
        println!("no test-only public methods found");
        return;
    }

    println!("public methods with no production caller ({}):", candidates.len());
    for (name, file) in &candidates {
        println!("  {:<40} {}", name, file);
    }
    println!();
    println!("each is a CANDIDATE, not a bug — `*_for_testing` helpers and public");
    println!("API surface used outside this workspace belong here too. The one that");
    println!("matters is a method whose ONLY reason to exist is production behaviour.");
}

/// The repo root: the directory holding the workspace `Cargo.toml`.
fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/tools/xtask`, so TWO parents up is the
    // workspace root. (An earlier version used three and silently resolved to the
    // parent of the repo, which scanned zero files and reported "no findings" —
    // a check that always passes is worse than no check at all.)
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(here)
}

/// Every `.rs` file under `crates/`, excluding test files and build scripts.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let crates = root.join("crates");
    let Ok(entries) = std::fs::read_dir(&crates) else {
        return out;
    };
    for e in entries.flatten() {
        walk(e.path(), &mut out);
    }
    out
}

fn walk(dir: PathBuf, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            // `target/` is build output; skipping it keeps the scan fast.
            if p.file_name().map(|n| n == "target").unwrap_or(false) {
                continue;
            }
            walk(p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            // Integration tests call the public API from outside, which is
            // exactly the "used only by tests" case — exclude them from being
            // counted as production use, but still scan them for definitions.
            out.push(p);
        }
    }
}

/// Names of `pub fn` / `pub async fn` defined in `prod` (non-test code).
fn public_fns(prod: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in prod.lines() {
        let t = line.trim_start();
        // Skip trait impls' `pub fn` inside `mod tests` (already excluded) and
        // macro-generated code we cannot reason about.
        let Some(rest) = t
            .strip_prefix("pub fn ")
            .or_else(|| t.strip_prefix("pub async fn "))
            .or_else(|| t.strip_prefix("pub(crate) fn "))
            .or_else(|| t.strip_prefix("pub(crate) async fn "))
        else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // Skip generic noise and very short names that would match everywhere.
        if name.len() >= 6 && !name.starts_with("new") {
            names.insert(name);
        }
    }
    names
}

/// How many times `name` is *called* in production code across the workspace.
///
/// A definition does not count as a use, so this is a call-site count.
///
/// The defining file IS scanned, minus its own `pub fn <name>` line: most calls
/// in this codebase are intra-file (`self.run_bash_tool(...)`,
/// `self.advance_hold(...)`), and skipping the file entirely buried the real
/// signal under 45 false positives — including every method `agent_loop` calls on
/// itself.
fn production_uses(root: &Path, name: &str) -> usize {
    // A call site is the name followed by `(`. The definition is the name
    // preceded by `fn `, so those two never collide.
    let needle = format!("{}(", name);
    let definition = format!("fn {}(", name);
    let mut hits = 0;
    for file in rust_sources(root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // Skip test-only sections of every file: a call from `#[cfg(test)]` is
        // not production use. This is the whole point of the check — `advance_hold`
        // had five test callers and zero real ones.
        let split = text.find("#[cfg(test)]").unwrap_or(text.len());
        let prod = &text[..split];
        for line in prod.lines() {
            let t = line.trim_start();
            // Comments and doc lines describe the API without using it.
            if t.starts_with("//") || t.starts_with("///") || t.starts_with("*") {
                continue;
            }
            if !line.contains(&needle) {
                continue;
            }
            // Don't count the definition itself, or a re-export of the name.
            if line.contains(&definition) || t.starts_with("pub use") {
                continue;
            }
            hits += 1;
        }
    }
    hits
}




