//! Run configuration, mirroring `cmd/swarm/runner.go`'s `runConfig` and
//! `defaultRunConfig` (L25-114) one field at a time.
//!
//! The defaults here are the shipped Go defaults. A test asserts each one so a
//! drift shows up as a failure rather than as a behaviour change in production.

use std::time::Duration;

/// All CLI knobs for `swarm run`.
///
/// Field-for-field mirror of Go's `runConfig`. Durations that Go accepts as
/// `time.Duration` strings (`20m`, `1h30m`) are `Duration` here; a zero value
/// keeps its documented meaning (see each field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfig {
    pub max_workers: usize,
    pub min_workers: usize,
    pub initial_workers: usize,
    pub max_turns: u32,
    /// `turn > max_turns * hard_turn_factor` retires a worker unconditionally.
    pub hard_turn_factor: u32,
    pub idle_turns_to_exit: u32,
    pub lease_sec: u64,
    pub max_items: i64,
    pub max_item_turns: u32,
    pub max_item_tokens: i64,
    pub budget_tokens: i64,
    pub max_item_fails: u32,
    /// Park a worker as crashed after this many consecutive turns with no
    /// submission and no idle. See the no-progress guard.
    pub max_no_progress_turns: u32,
    pub test_cmd: String,
    pub mission: String,
    pub conventions: String,
    pub server_port: u16,
    /// `echo` (offline) or `direct` (OpenAI-compatible).
    pub harness_name: String,
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub exit_when_done: bool,
    /// Forbid workers creating new items (`--no-create`).
    pub no_create: bool,
    /// Hard ceiling for the whole run; `Duration::ZERO` disables it.
    pub max_wall_clock: Duration,
    /// Release an item's lease when one worker has held it this long;
    /// `0` disables it.
    pub max_item_seconds: u64,
    /// Stop after this long with NO progress; `Duration::ZERO` means
    /// "use the default" (10 minutes).
    pub idle_stop: Duration,
    /// Load cross-run memory into every worker prompt, and write back what the
    /// run learned. `--no-memory` clears it.
    ///
    /// Off means the run is reproducible from the repo alone: no notes in, no
    /// notes out. Useful when a stale note would be worse than no note, and for
    /// benchmarks that must be comparable between runs.
    pub memory_enabled: bool,
    /// Where the memory file lives. Empty means `MemoryStore::default_path()`.
    pub memory_path: String,
}

impl Default for RunConfig {
    /// The shipped defaults, copied from Go `defaultRunConfig` (L93-114).
    fn default() -> Self {
        Self {
            max_workers: 1000,
            min_workers: 1,
            initial_workers: 8,
            max_turns: 10,
            hard_turn_factor: 6,
            idle_turns_to_exit: 2,
            lease_sec: 300,
            max_items: 200,
            max_item_turns: 6,
            max_item_tokens: 2_000_000,
            budget_tokens: 0,
            max_item_fails: 3,
            max_no_progress_turns: 3,
            test_cmd: String::new(),
            mission: String::new(),
            conventions: String::new(),
            server_port: 17771,
            harness_name: "direct".to_string(),
            api_base: String::new(),
            api_key: String::new(),
            model: String::new(),
            exit_when_done: true,
            no_create: false,
            max_wall_clock: Duration::from_secs(6 * 3600),
            max_item_seconds: 300,
            idle_stop: Duration::from_secs(10 * 60),
            memory_enabled: true,
            memory_path: String::new(),
        }
    }
}

/// Default idle-stop used when the configured value is zero.
pub const DEFAULT_IDLE_STOP: Duration = Duration::from_secs(10 * 60);

impl RunConfig {
    /// Effective idle-stop, applying the "0 means default" rule.
    pub fn effective_idle_stop(&self) -> Duration {
        if self.idle_stop.is_zero() {
            DEFAULT_IDLE_STOP
        } else {
            self.idle_stop
        }
    }

    /// Lease in milliseconds, as the store expects.
    pub fn lease_millis(&self) -> i64 {
        (self.lease_sec as i64) * 1000
    }

    /// Hard turn ceiling for one worker (`max_turns * hard_turn_factor`).
    pub fn hard_max_turns(&self) -> u32 {
        self.max_turns.saturating_mul(self.hard_turn_factor)
    }

    /// Apply the `--exit-when-done` interaction with `min_workers`.
    ///
    /// Go does this in `main.go` L182-184: exit-when-done drops the idle-retire
    /// floor so the LAST worker can leave once the seed work is terminal.
    /// Without it, min_workers=1 keeps one worker spinning on an empty board
    /// (measured: ~90s of pointless turns after everything was done).
    pub fn apply_exit_when_done(&mut self) {
        if self.exit_when_done {
            self.min_workers = 0;
        }
    }
}

/// Parsed `swarm run` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub dir: String,
    pub config: RunConfig,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("{0} looks like a flag, not a directory")]
    LooksLikeFlag(String),
    #[error("missing value for {0}")]
    MissingValue(String),
    #[error("{flag} {value:?}: {reason}")]
    BadValue {
        flag: String,
        value: String,
        reason: String,
    },
}

/// Parse the arguments that follow `swarm run`.
///
/// Mirrors `main.go` L72-188, including the guard that rejects a `-`-prefixed
/// directory: without it `swarm run --help` silently created
/// `./--help/.swarm/worktrees/wN`, polluting the worktree registry and making
/// syncWorktree misbehave (workers claimed items but never called the LLM).
pub fn parse_run_args(args: &[String]) -> Result<RunArgs, ParseError> {
    let dir = args
        .first()
        .cloned()
        .ok_or_else(|| ParseError::MissingValue("dir".to_string()))?;
    if dir.starts_with('-') {
        return Err(ParseError::LooksLikeFlag(dir));
    }

    let mut cfg = RunConfig::default();
    let mut i = 1;
    // helper to fetch the value that follows a flag
    macro_rules! val {
        () => {{
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| ParseError::MissingValue(args[i - 1].clone()))?
        }};
    }

    while i < args.len() {
        match args[i].as_str() {
            "--max-workers" => cfg.max_workers = parse_num(&args[i], &val!())?,
            "--min-workers" => cfg.min_workers = parse_num(&args[i], &val!())?,
            "--workers" => cfg.initial_workers = parse_num(&args[i], &val!())?,
            "--max-turns" => cfg.max_turns = parse_num(&args[i], &val!())?,
            "--lease-sec" => cfg.lease_sec = parse_num(&args[i], &val!())?,
            "--max-items" => cfg.max_items = parse_num(&args[i], &val!())?,
            "--max-item-turns" => cfg.max_item_turns = parse_num(&args[i], &val!())?,
            "--max-item-tokens" => cfg.max_item_tokens = parse_num(&args[i], &val!())?,
            "--max-item-fails" => cfg.max_item_fails = parse_num(&args[i], &val!())?,
            "--max-no-progress-turns" => cfg.max_no_progress_turns = parse_num(&args[i], &val!())?,
            "--budget-tokens" => cfg.budget_tokens = parse_num(&args[i], &val!())?,
            "--server-port" => cfg.server_port = parse_num(&args[i], &val!())?,
            "--harness" => cfg.harness_name = val!(),
            "--api-base" => cfg.api_base = val!(),
            "--api-key" => cfg.api_key = val!(),
            // Key without putting it in argv. A key passed as `--api-key <value>`
            // lands in the process table and the shell history, which on a shared
            // host is a real leak. `--api-key-file` reads a path; `--api-key-stdin`
            // reads the value from stdin. Both keep the key out of `ps` output.
            "--api-key-file" => {
                let path = val!();
                let key = std::fs::read_to_string(&path)
                    .map_err(|e| ParseError::BadValue {
                        flag: "--api-key-file".into(),
                        value: path.clone(),
                        reason: format!("cannot read: {}", e),
                    })?;
                // Tolerate a trailing newline: `echo key > f` is the common case.
                cfg.api_key = key.trim().to_string();
            }
            "--api-key-stdin" => {
                let mut key = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut key).map_err(|e| {
                    ParseError::BadValue {
                        flag: "--api-key-stdin".into(),
                        value: String::new(),
                        reason: format!("cannot read stdin: {}", e),
                    }
                })?;
                cfg.api_key = key.trim().to_string();
            }
            "--model" => cfg.model = val!(),
            "--mission" => cfg.mission = val!(),
            "--conventions" => cfg.conventions = val!(),
            "--test-cmd" => cfg.test_cmd = val!(),
            "--exit-when-done" => cfg.exit_when_done = true,
            "--no-exit-when-done" => cfg.exit_when_done = false,
            "--no-create" => cfg.no_create = true,
            "--no-memory" => cfg.memory_enabled = false,
            "--memory-path" => cfg.memory_path = val!(),
            "--max-wall-clock" => {
                let raw = val!();
                // "0" disables the ceiling; otherwise Go duration syntax.
                cfg.max_wall_clock = if raw == "0" {
                    Duration::ZERO
                } else {
                    parse_go_duration(&raw).map_err(|e| ParseError::BadValue {
                        flag: "--max-wall-clock".into(),
                        value: raw.clone(),
                        reason: e,
                    })?
                };
            }
            "--max-item-seconds" => cfg.max_item_seconds = parse_num(&args[i], &val!())?,
            "--idle-stop" => {
                let raw = val!();
                cfg.idle_stop = parse_go_duration(&raw).map_err(|e| ParseError::BadValue {
                    flag: "--idle-stop".into(),
                    value: raw.clone(),
                    reason: e,
                })?;
            }
            other => {
                // Unknown flags are ignored by the Go CLI (it only switches on
                // known names), so keep that lenient behaviour.
                let _ = other;
            }
        }
        i += 1;
    }

    cfg.apply_exit_when_done();
    Ok(RunArgs { dir, config: cfg })
}

fn parse_num<T: std::str::FromStr>(flag: &str, raw: &str) -> Result<T, ParseError> {
    raw.parse::<T>().map_err(|_| ParseError::BadValue {
        flag: flag.to_string(),
        value: raw.to_string(),
        reason: "not a number".to_string(),
    })
}

/// Parse a Go `time.ParseDuration` string: `90s`, `20m`, `1h30m`, `500ms`.
///
/// Only the units Go accepts are supported, and the grammar matches Go's
/// (sequence of `<number><unit>`, optional leading sign).
pub fn parse_go_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if rest == "0" {
        return Ok(Duration::ZERO);
    }

    let bytes = rest.as_bytes();
    let mut idx = 0;
    let mut total_nanos: f64 = 0.0;
    let mut matched_any = false;

    while idx < bytes.len() {
        // number (integer or decimal)
        let num_start = idx;
        while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == b'.') {
            idx += 1;
        }
        if num_start == idx {
            return Err(format!("invalid duration {:?}", s));
        }
        let num: f64 = rest[num_start..idx]
            .parse()
            .map_err(|_| format!("invalid number in duration {:?}", s))?;

        // unit
        let unit_start = idx;
        while idx < bytes.len() && !bytes[idx].is_ascii_digit() && bytes[idx] != b'.' {
            idx += 1;
        }
        let unit = &rest[unit_start..idx];
        let mult_nanos: f64 = match unit {
            "ns" => 1.0,
            "us" | "µs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60.0 * 1_000_000_000.0,
            "h" => 3600.0 * 1_000_000_000.0,
            other => return Err(format!("unknown unit {:?} in duration {:?}", other, s)),
        };
        total_nanos += num * mult_nanos;
        matched_any = true;
    }

    if !matched_any {
        return Err(format!("invalid duration {:?}", s));
    }
    if total_nanos < 0.0 || !total_nanos.is_finite() {
        return Err(format!("duration out of range: {:?}", s));
    }
    let nanos = total_nanos as u64;
    let d = Duration::from_nanos(nanos);
    Ok(if neg {
        // Go allows negative durations; the callers treat <=0 as "disabled",
        // so clamping to zero keeps the semantics without an error.
        Duration::ZERO
    } else {
        d
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every default must match Go `defaultRunConfig` exactly. A drift here is
    /// a behaviour change, not a refactor.
    #[test]
    fn defaults_match_go() {        let c = RunConfig::default();
        assert_eq!(c.max_workers, 1000);
        assert_eq!(c.min_workers, 1);
        assert_eq!(c.initial_workers, 8);
        assert_eq!(c.max_turns, 10);
        assert_eq!(c.hard_turn_factor, 6);
        assert_eq!(c.idle_turns_to_exit, 2);
        assert_eq!(c.lease_sec, 300);
        assert_eq!(c.max_items, 200);
        assert_eq!(c.max_item_turns, 6);
        assert_eq!(c.max_item_tokens, 2_000_000);
        assert_eq!(c.max_item_fails, 3);
        assert_eq!(c.max_no_progress_turns, 3);
        assert_eq!(c.server_port, 17771);
        assert_eq!(c.harness_name, "direct");
        assert!(c.exit_when_done);
        assert!(!c.no_create);
        assert_eq!(c.max_wall_clock, Duration::from_secs(6 * 3600));
        assert_eq!(c.max_item_seconds, 300);
        assert_eq!(c.idle_stop, Duration::from_secs(600));
        assert_eq!(c.budget_tokens, 0);
    }

    #[test]
    fn rejects_flag_as_directory() {
        let args: Vec<String> = vec!["--help".into()];
        assert_eq!(
            parse_run_args(&args),
            Err(ParseError::LooksLikeFlag("--help".into()))
        );
    }

    #[test]
    fn exit_when_done_zeroes_min_workers() {
        let mut c = RunConfig::default();
        c.exit_when_done = true;
        c.apply_exit_when_done();
        assert_eq!(c.min_workers, 0);

        let mut c2 = RunConfig::default();
        c2.exit_when_done = false;
        c2.apply_exit_when_done();
        assert_eq!(c2.min_workers, 1);
    }

    #[test]
    fn parses_durations_like_go() {
        assert_eq!(parse_go_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_go_duration("20m").unwrap(), Duration::from_secs(1200));
        assert_eq!(parse_go_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_go_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_go_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_go_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_go_duration("abc").is_err());
        assert!(parse_go_duration("10").is_err(), "bare number needs a unit");
        assert!(parse_go_duration("10x").is_err());
    }

    #[test]
    fn parses_flags() {
        let args: Vec<String> = vec![
            "proj".into(),
            "--workers".into(),
            "3".into(),
            "--max-item-seconds".into(),
            "120".into(),
            "--model".into(),
            "m1".into(),
            "--no-create".into(),
            "--idle-stop".into(),
            "5m".into(),
        ];
        let a = parse_run_args(&args).unwrap();
        assert_eq!(a.dir, "proj");
        assert_eq!(a.config.initial_workers, 3);
        assert_eq!(a.config.max_item_seconds, 120);
        assert_eq!(a.config.model, "m1");
        assert!(a.config.no_create);
        assert_eq!(a.config.idle_stop, Duration::from_secs(300));
    }

    #[test]
    fn max_wall_clock_zero_disables() {
        let args: Vec<String> = vec!["p".into(), "--max-wall-clock".into(), "0".into()];
        let a = parse_run_args(&args).unwrap();
        assert!(a.config.max_wall_clock.is_zero());
    }

    #[test]
    fn hard_max_turns_is_product() {
        let c = RunConfig::default();
        assert_eq!(c.hard_max_turns(), 60);
    }

    #[test]
    fn effective_idle_stop_falls_back() {
        let mut c = RunConfig::default();
        c.idle_stop = Duration::ZERO;
        assert_eq!(c.effective_idle_stop(), DEFAULT_IDLE_STOP);
    }

    /// `--api-key-file` reads the key from a path, keeping it out of argv.
    ///
    /// A key on the command line is visible to `ps` and lands in shell history.
    /// cc-fleet's invariant "keys never in env/argv/history" is the same rule;
    /// the file form is the cheapest way to honour it for a local tool.
    #[test]
    fn api_key_file_keeps_the_key_out_of_argv() {
        let mut path = std::env::temp_dir();
        path.push(format!("swarm-key-{}.txt", std::process::id()));
        // A trailing newline is what `echo key > f` produces; it must be
        // tolerated or every key would carry a stray \n into the header.
        std::fs::write(&path, "sk-test-key\n").unwrap();

        let args = vec![
            "dir".to_string(),
            "--api-key-file".to_string(),
            path.to_string_lossy().to_string(),
        ];
        let parsed = parse_run_args(&args).expect("parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.config.api_key, "sk-test-key");
    }

    /// A missing key file is a parse error, not an empty key that fails later
    /// with a confusing 401.
    #[test]
    fn api_key_file_missing_is_an_error() {
        let args = vec![
            "dir".to_string(),
            "--api-key-file".to_string(),
            "C:\\does\\not\\exist\\key.txt".to_string(),
        ];
        let err = parse_run_args(&args).unwrap_err();
        assert!(
            matches!(err, ParseError::BadValue { .. }),
            "a missing key file must fail at parse time, got {:?}",
            err
        );
    }

    /// `--api-key` still works (the Go CLI had it), so existing scripts do not
    /// break — but the two new forms are the ones that do not leak.
    #[test]
    fn api_key_flag_still_parses() {
        let args = vec![
            "dir".to_string(),
            "--api-key".to_string(),
            "sk-inline".to_string(),
        ];
        let parsed = parse_run_args(&args).expect("parse");
        assert_eq!(parsed.config.api_key, "sk-inline");
    }
}
