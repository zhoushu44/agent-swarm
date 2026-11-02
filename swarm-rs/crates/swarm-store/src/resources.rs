//! Resource-token extraction, mirrored from Go `internal/store/items.go`
//! L27-92 and `claims.go` L149-184.
//!
//! The regex and the extension whitelist are the whole point: they are what
//! stop prose like `error.message` from becoming a "resource". Two consumers
//! share them, so they must agree exactly —
//!
//!   * `artifact_key` (the item-level dedup key, `f:<res>|<res>`)
//!   * `item_resources` (the stigmergy conflict scan and the split axis)
//!
//! If these two ever disagree, a worker can be told "no conflict" and then have
//! its merge rejected for touching the same file.

use once_cell::sync::Lazy;
use regex::Regex;

/// The resource regex, copied verbatim from Go `items.go` L27.
///
/// `(?:[A-Za-z0-9_.@-]+/)*` — optional path segments
/// `[A-Za-z0-9_@.-]+\.[A-Za-z][A-Za-z0-9]{0,6}` — filename + extension
/// `(?:#[A-Za-z_][A-Za-z0-9_]*)?` — optional `#symbol` suffix
pub static RESOURCE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?:[A-Za-z0-9_.@-]+/)*[A-Za-z0-9_@.-]+\.[A-Za-z][A-Za-z0-9]{0,6}(?:#[A-Za-z_][A-Za-z0-9_]*)?",
    )
    .expect("resource regex is a compile-time constant")
});

/// Extensions accepted without requiring a slash in the token.
///
/// Keeping this explicit is what stops `error.message` from becoming a
/// resource: "message" is not a known extension, so the token is rejected.
/// Copied from Go `items.go` L32-44.
const KNOWN_EXT: &[&str] = &[
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "go", "py", "rs", "java", "kt", "rb", "php", "c", "h",
    "cc", "cpp", "hpp", "cs", "swift", "scala", "sh", "bash", "ps1", "json", "yaml", "yml", "toml",
    "ini", "env", "md", "txt", "rst", "html", "htm", "css", "scss", "less", "vue", "svelte", "xml",
    "svg", "sql", "db", "csv", "tsv", "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip",
    "tar", "gz", "lock", "log", "test", "spec", "mod", "sum", "work",
];

/// Path heuristic for one candidate token (Go `items.go` L47-66).
pub fn looks_like_resource(tok: &str) -> bool {
    let t = tok.trim().replace('\\', "/");
    if t.is_empty() {
        return false;
    }
    let dot = match t.rfind('.') {
        Some(d) => d,
        None => return false,
    };
    if dot == 0 || dot == t.len() - 1 {
        return false;
    }
    // A slash anywhere means it is a path: "src/util.js", "./a.js", "a/b/c.d".
    if t.contains('/') {
        return true;
    }
    // Otherwise the extension must be a known file extension.
    let mut ext = t[dot + 1..].to_lowercase();
    if let Some(h) = ext.find('#') {
        ext.truncate(h);
    }
    KNOWN_EXT.contains(&ext.as_str())
}

/// Normalize one matched token the way both Go call sites do:
/// lowercase, backslashes to slashes, strip a leading `./`, trim trailing
/// punctuation.
fn normalize(tok: &str) -> String {
    let mut n = tok.replace('\\', "/").to_lowercase();
    if let Some(rest) = n.strip_prefix("./") {
        n = rest.to_string();
    }
    n.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | ')'))
        .to_string()
}

/// The item-level dedup key: `f:<res>|<res>` or empty.
///
/// Go `items.go` L70-92.
pub fn artifact_key(title: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    for m in RESOURCE_RE.find_iter(title) {
        let raw = m.as_str();
        if !looks_like_resource(raw) {
            continue;
        }
        let n = normalize(raw);
        if n.is_empty() || seen.iter().any(|s| s == &n) {
            continue;
        }
        seen.push(n);
    }
    if seen.is_empty() {
        return String::new();
    }
    format!("f:{}", seen.join("|"))
}

/// Resource tokens (path or path#symbol) an item names, lowercased.
///
/// Scans title then detail, dedups across both, and caps at 12 — an item naming
/// 40 files should not trigger a 40-way conflict scan on every claim
/// (Go `claims.go` L157-184).
pub fn item_resources(title: &str, detail: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for text in [title, detail] {
        for m in RESOURCE_RE.find_iter(text) {
            let raw = m.as_str();
            if !looks_like_resource(raw) {
                continue;
            }
            let n = normalize(raw);
            if n.is_empty() || seen.iter().any(|s| s == &n) {
                continue;
            }
            seen.push(n);
            if seen.len() > 12 {
                seen.truncate(12);
                return seen;
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_is_not_a_resource() {
        // "error.message" — "message" is not a known extension.
        assert!(!looks_like_resource("error.message"));
        assert_eq!(artifact_key("handle the error.message case"), "");
    }

    #[test]
    fn known_extension_without_slash_is_a_resource() {
        assert!(looks_like_resource("server.js"));
        assert!(looks_like_resource("main.go"));
        assert!(looks_like_resource("Cargo.toml"));
    }

    #[test]
    fn slash_makes_anything_a_path() {
        assert!(looks_like_resource("src/whatever.xyz"));
        assert!(looks_like_resource("./a.js"));
    }

    #[test]
    fn symbol_suffix_is_kept() {
        let r = item_resources("fix src/a.js#buildVideoBody", "");
        assert_eq!(r, vec!["src/a.js#buildvideoBody".to_lowercase()]);
    }

    #[test]
    fn symbol_granularity_keeps_same_file_apart() {
        // The whole point of #symbol: two functions of one file do not collide.
        let a = item_resources("src/dola/video.js#pollVideo", "");
        let b = item_resources("src/dola/video.js#buildVideoBody", "");
        assert_ne!(a, b);
    }

    #[test]
    fn artifact_key_sorts_and_joins_in_order() {
        let k = artifact_key("edit src/a.js and src/b.js");
        assert_eq!(k, "f:src/a.js|src/b.js");
    }

    #[test]
    fn artifact_key_dedups() {
        let k = artifact_key("src/a.js and src/a.js again");
        assert_eq!(k, "f:src/a.js");
    }

    /// Windows paths: the regex path segment class is `[A-Za-z0-9_.@-]`, which
    /// does NOT include `\`. So a backslash path only yields its basename.
    ///
    /// This reproduces Go's behaviour exactly (its `ReplaceAll(m, "\\", "/")`
    /// is dead code for paths for the same reason) — see
    /// `docs/OPTIMIZATION-SWARM.md` §4.1: on Windows a worker writing
    /// `src\openai\http.js` is deduped as `http.js`, so two different
    /// directories collide. Fixing it means changing the regex, which changes
    /// dedup behaviour, so it is a deliberate decision rather than a port bug.
    #[test]
    fn backslash_paths_only_yield_the_basename() {
        let r = item_resources(r"fix src\openai\http.js", "");
        assert_eq!(r, vec!["http.js"], "matches Go: backslash is not a path char");
    }

    #[test]
    fn leading_dot_slash_is_stripped() {
        let r = item_resources("./a.js", "");
        assert_eq!(r, vec!["a.js"]);
    }

    #[test]
    fn trailing_punctuation_is_trimmed() {
        let r = item_resources("see src/a.js.", "");
        assert_eq!(r, vec!["src/a.js"]);
    }

    #[test]
    fn caps_at_twelve() {
        let title: String = (0..30)
            .map(|i| format!("src/f{}.js", i))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(item_resources(&title, "").len(), 12);
    }

    #[test]
    fn dedups_across_title_and_detail() {
        let r = item_resources("src/a.js", "also src/a.js");
        assert_eq!(r, vec!["src/a.js"]);
    }
}
