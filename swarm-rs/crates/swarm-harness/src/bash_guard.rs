//! Bash guard: refuse commands that edit files through a script or an inline
//! interpreter instead of the `write` tool.
//!
//! Measured on a real run: workers wrote `tmp_patch.mjs`, `tmp_patch2.mjs`,
//! `tmp_t68_patch.js`, `tmp_addtests.js` and then ran `node tmp_patch.mjs` to
//! edit source files. Each edit cost ~4 tool calls (write script → run →
//! verify → delete) versus one `write` call. The prompt already says not to;
//! this is the tool-level counterpart, because a prompt rule is advisory and
//! the model still did it five times in fifteen minutes.
//!
//! The guard is deliberately narrow: it only blocks patterns that are
//! unambiguously "script edits a file". Running tests, builds, git, and
//! read-only `node -e` one-liners stay allowed.

use once_cell::sync::Lazy;
use regex::Regex;

/// Interpreter invoked on a worker-written helper script.
///
/// The name patterns are what distinguish `node tmp_patch.mjs` (blocked) from
/// `node src/server.js` (the project itself — allowed): patch scripts are always
/// `tmp_*`, `_*`, `scratch/`, `debug_`, `probe.`, or named after what they do
/// (patch/fix/edit), because that is what a worker improvises.
static SCRIPT_RUNNER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(node|python|python3|py|deno|bun|ts-node|perl|ruby|powershell|pwsh)\s+[^\s|;&]*[/\\]?(tmp_|_|scratch/|debug_|probe\.|patch|fix|edit)[^\s|;&]*\.(mjs|cjs|js|ts|py|pl|rb|ps1)\b",
    )
    .expect("script runner regex")
});

/// Interpreter flags that execute a passed-in program.
static INLINE_EDIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(node|python3?|py|perl|ruby)\s+(-e|--eval|-c)\b").expect("inline edit regex")
});

/// Calls that mean the inline program mutates files.
static FS_WRITE_HINT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(writeFileSync|writeFile|appendFileSync|appendFile|createWriteStream|rmSync|unlinkSync|mkdirSync|renameSync|copyFileSync|\bopen\s*\([^)]*['"][wax]\+?['"])"#,
    )
    .expect("fs write hint regex")
});

/// Classic in-place editors.
static INPLACE_EDITORS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(sed|perl)\s+(-i|-[a-z]*i)").expect("inplace regex"));

/// Shell redirection into a source-ish file.
static REDIRECT_TO_SOURCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#">{1,2}\s*["']?[^\s|&;"']+\.(mjs|cjs|js|ts|jsx|tsx|go|py|java|c|h|cc|cpp|rs|rb|php|cs|swift|kt|scala|json|ya?ml|toml|md|css|html?|xml|sql)\b"#,
    )
    .expect("redirect regex")
});

/// Redirections that are not source edits.
static SAFE_SINK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)>{1,2}\s*/dev/null\b").expect("safe sink regex"));

/// Extensions that are output/scratch, not source.
const SCRATCH_EXTS: &[&str] = &[".txt", ".log", ".out", ".tmp", ".json.tmp"];

const GUARD_HINT: &str = "Use the write tool to change file contents (one call), not a script. \
bash is for RUNNING things: tests, builds, git, reads.";

/// The outcome of checking one bash command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardVerdict {
    pub blocked: bool,
    pub reason: String,
    pub hint: String,
}

impl GuardVerdict {
    fn block(reason: String) -> Self {
        Self {
            blocked: true,
            reason,
            hint: GUARD_HINT.to_string(),
        }
    }
}

/// Report whether `cmd` should be refused for editing files through a script or
/// an inline interpreter.
pub fn check_bash_command(cmd: &str) -> GuardVerdict {
    let c = cmd.trim();
    if c.is_empty() {
        return GuardVerdict::default();
    }
    let lower = c.to_lowercase();

    // 1) interpreter + a script file: node tmp_patch.mjs, python3 fix.py
    if let Some(m) = SCRIPT_RUNNER.find(c) {
        return GuardVerdict::block(format!(
            "running a script to edit files: {}",
            m.as_str().trim()
        ));
    }

    // 2) in-place editors: sed -i, perl -pi -e
    if let Some(m) = INPLACE_EDITORS.find(c) {
        return GuardVerdict::block(format!("in-place file editing: {}", m.as_str().trim()));
    }

    // 3) inline -e/-c program that writes files: node -e "fs.writeFileSync(...)"
    if INLINE_EDIT.is_match(&lower) && FS_WRITE_HINT.is_match(c) {
        return GuardVerdict::block("inline interpreter code that writes files".to_string());
    }

    // 4) shell redirection into a source file: echo ... > src/x.js
    //    (allowed: > /dev/null, > *.txt, > *.log, >> into logs)
    if let Some(why) = check_redirect(&lower) {
        return GuardVerdict::block(why);
    }

    GuardVerdict::default()
}

fn check_redirect(lower: &str) -> Option<String> {
    // Strip the safe sinks first so `cmd > /dev/null 2>&1` never trips.
    let mut cleaned = SAFE_SINK.replace_all(lower, "").to_string();
    // Output/scratch files are how workers inspect things; allow them.
    for ext in SCRATCH_EXTS {
        cleaned = cleaned.replace(ext, " ");
    }
    REDIRECT_TO_SOURCE
        .find(&cleaned)
        .map(|m| format!("shell redirection into a source file: {}", m.as_str().trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(cmd: &str) -> bool {
        check_bash_command(cmd).blocked
    }

    #[test]
    fn blocks_script_patch_of_source() {
        assert!(blocked("node tmp_patch.mjs"));
        assert!(blocked("node tmp_patch2.mjs"));
        assert!(blocked("node tmp_t68_patch.js"));
        assert!(blocked("python3 fix.py"));
        assert!(blocked("node scratch/x.js"));
        assert!(blocked("node debug_run.js"));
    }

    /// Shell interpreters are NOT in the guard's pattern list, so `bash
    /// tmp_patch.sh` slips through — in Go too.
    ///
    /// Verified against the Go binary: `CheckBashCommand("bash tmp_patch.sh")`
    /// returns `blocked=false`. This test pins the current (leaky) behaviour so
    /// the port stays faithful; tightening it is a deliberate change recorded
    /// in `docs/OPTIMIZATION-SWARM.md` §4.2.
    #[test]
    fn shell_interpreters_are_not_blocked_matching_go() {
        assert!(
            !blocked("bash tmp_patch.sh"),
            "matches Go: bash/sh are absent from the interpreter list"
        );
        assert!(!blocked("sh scratch/do.sh"));
    }

    /// `bash -c` with a redirect into source IS caught, because the redirect
    /// rule does not care which shell produced it.
    #[test]
    fn bash_c_with_redirect_into_source_is_blocked() {
        assert!(blocked(r#"bash -c 'echo x > src/a.js'"#));
    }

    #[test]
    fn allows_running_the_project_itself() {
        assert!(!blocked("node src/server.js"), "project entrypoint is allowed");
        assert!(!blocked("node index.js"));
        assert!(!blocked("go test ./..."));
        assert!(!blocked("cargo test"));
        assert!(!blocked("npm run build"));
        assert!(!blocked("git status"));
        assert!(!blocked("git diff --stat"));
    }

    #[test]
    fn blocks_inplace_editors() {
        assert!(blocked("sed -i 's/a/b/' src/x.js"));
        assert!(blocked("perl -pi -e 's/a/b/' src/x.js"));
        assert!(!blocked("sed -n '1,10p' src/x.js"), "reading is fine");
    }

    #[test]
    fn blocks_inline_program_that_writes() {
        assert!(blocked(
            r#"node -e "require('fs').writeFileSync('src/a.js','x')""#
        ));
        assert!(
            !blocked(r#"node -e "console.log(1)""#),
            "read-only inline program is allowed"
        );
    }

    #[test]
    fn blocks_redirect_into_source() {
        assert!(blocked("echo 'x' > src/x.js"));
        assert!(blocked("cat a >> src/y.ts"));
    }

    #[test]
    fn allows_redirect_into_scratch_and_devnull() {
        assert!(!blocked("echo hi > out.txt"));
        assert!(!blocked("go test ./... > /dev/null 2>&1"));
        assert!(!blocked("make 2>&1 > build.log"));
        assert!(!blocked("echo x > notes.out"));
    }

    #[test]
    fn empty_command_is_allowed() {
        assert!(!blocked(""));
        assert!(!blocked("   "));
    }

    #[test]
    fn verdict_carries_a_hint_when_blocked() {
        let v = check_bash_command("node tmp_patch.mjs");
        assert!(v.blocked);
        assert!(v.hint.contains("write tool"));
        assert!(v.reason.contains("tmp_patch.mjs"));
    }
}
