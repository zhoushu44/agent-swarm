//! Git plumbing for the swarm: one branch per worker, each checked out in its
//! own worktree under `<repo>/.swarm/worktrees/<worker>`, merged into an
//! `integration` branch that a gate command must approve.
//!
//! Mirrors Go `internal/gitutil`. The design constraint that shapes everything
//! here: **a merge is serial, a gate is parallel**. `git` itself serialises on
//! the integration worktree's index, but running the test suite can happen
//! concurrently — that is the Amdahl fix in `integratorLoop`.
//!
//! All git calls go through `std::process::Command` rather than a library
//! binding: the Go version shells out, the behaviour is well understood, and a
//! `git2` dependency would add ~200 transitive crates for no benefit here.

pub mod integrate;
pub mod repo;
pub mod worktree;

pub use integrate::MergeResult;
pub use repo::{ConflictFile, ErrGate, MergeResolver, Repo, RepoError, Worktree};

/// Truncate `s` to at most `max_bytes`, never splitting a UTF-8 char.
///
/// `String::truncate` takes a BYTE index and panics when the cut lands inside a
/// multi-byte sequence. Everything truncated here — command output, gate failure
/// text, a conflicted source file — routinely contains non-ASCII text, so a
/// byte-wise cut is a latent panic rather than a theoretical one.
///
/// The result may be a few bytes shorter than `max_bytes`; that is the point.
pub fn floor_to_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_never_splits_a_char() {
        // Each Chinese char is 3 bytes. Cutting at 10 bytes would land mid-char
        // and `String::truncate` would panic; the helper must land at 9.
        let mut s = "一二三四五六七八九十".to_string();
        floor_to_char_boundary(&mut s, 10);
        assert_eq!(s, "一二三", "3 chars = 9 bytes, the largest safe cut");
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn truncation_keeps_emoji_intact() {
        // 4-byte sequences: the same trap with a wider char.
        let mut s = "😀😁😂😃😄".to_string();
        floor_to_char_boundary(&mut s, 6);
        assert_eq!(s, "😀", "one 4-byte char, not a half of the second");
    }

    #[test]
    fn truncation_is_a_noop_below_the_limit() {
        let mut s = "short".to_string();
        floor_to_char_boundary(&mut s, 100);
        assert_eq!(s, "short");
    }

    #[test]
    fn truncation_handles_ascii_exactly() {
        let mut s = "abcdefghij".to_string();
        floor_to_char_boundary(&mut s, 4);
        assert_eq!(s, "abcd");
    }
}
