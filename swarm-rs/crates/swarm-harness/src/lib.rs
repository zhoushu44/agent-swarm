//! LLM harness: the 4-tool agent loop and the AIMD gate.
//!
//! The loop is the core of the swarm: a worker turn is a sequence of
//! `bash`/`write`/`http`/`finish` tool calls driven by an OpenAI-compatible
//! endpoint, and the loop only ends when the model calls `finish`, replies with
//! plain text *after* submitting, or hits the step cap.

pub mod agent_loop;
pub mod aimd;
pub mod bash_guard;
pub mod direct;
pub mod tools;

pub use agent_loop::{run_agent_loop, BoxFuture, ChatClient, ChatRequest, ChatResponse, HarnessError, LoopMessage, ToolCall};
pub use aimd::{AdaptiveGate, GateGuard, GateRunError};
pub use bash_guard::{check_bash_command, GuardVerdict};
pub use direct::{DirectConfig, DirectHarness};
pub use tools::{SWARM_TOOLS, TOOL_BASH, TOOL_FINISH, TOOL_HTTP, TOOL_WRITE};

use std::time::Duration;

/// Default cap on tool-call rounds per turn (Go `AgentLoopConfig.MaxSteps`).
pub const DEFAULT_MAX_STEPS: u32 = 40;

/// Default bash timeout (Go `AgentLoopConfig.BashTimeout`).
pub const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_secs(60);

/// How many times a plain-text (no tool call) reply is re-primed before giving
/// up (Go `maxPlainTextRetries`).
pub const MAX_PLAIN_TEXT_RETRIES: u32 = 3;

/// Token cap for one agent-loop round-trip.
pub const LOOP_MAX_TOKENS: u32 = 4096;

/// Token cap for a merge-arbitration call.
pub const ARBITRATE_MAX_TOKENS: u32 = 8192;

/// The re-prime message sent when the model answers in prose without calling a
/// tool and nothing was submitted.
///
/// Copied verbatim from Go `agentloop.go` L214-216: some endpoints ignore
/// `tool_choice: "required"` and answer in plain text when they judge "no tool
/// is needed". Treating that as completion makes the worker exit having done
/// nothing, so it is re-primed instead.
pub const REPRIME_MESSAGE: &str = "You have not called any tool yet and nothing was submitted. Plain text does not complete the task. You MUST call a tool now: use bash/write to make progress, http to claim or submit work, or finish(summary) only when the work is actually done.";

/// Configuration for one agent loop run.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    /// Working directory for `bash` / `write` (typically the worker worktree).
    pub work_dir: String,
    /// Local swarm server port the `http` tool targets.
    pub server_port: u16,
    /// Caps tool-call rounds.
    pub max_steps: u32,
    /// Bounds one bash command.
    pub bash_timeout: Duration,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            work_dir: String::new(),
            server_port: 0,
            max_steps: DEFAULT_MAX_STEPS,
            bash_timeout: DEFAULT_BASH_TIMEOUT,
        }
    }
}

impl AgentLoopConfig {
    /// Apply defaults to zero fields (Go `normalize`).
    pub fn normalized(mut self) -> Self {
        if self.max_steps == 0 {
            self.max_steps = DEFAULT_MAX_STEPS;
        }
        if self.bash_timeout.is_zero() {
            self.bash_timeout = DEFAULT_BASH_TIMEOUT;
        }
        self
    }
}

/// Token usage for one turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

/// Result of one agent loop run.
#[derive(Debug, Clone, Default)]
pub struct AgentLoopResult {
    /// The model's closing text (or `(no reply)`).
    pub reply: String,
    pub usage: Usage,
    /// HTTP rounds actually performed (not turns).
    pub steps: u32,
    /// Whether a submission was made during the loop.
    pub submitted: bool,
}

/// Replace an empty reply with the Go sentinel `(no reply)`.
pub fn or_no_reply(s: &str) -> String {
    if s.trim().is_empty() {
        "(no reply)".to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_go() {
        let c = AgentLoopConfig::default();
        assert_eq!(c.max_steps, 40);
        assert_eq!(c.bash_timeout, Duration::from_secs(60));
        assert_eq!(MAX_PLAIN_TEXT_RETRIES, 3);
        assert_eq!(LOOP_MAX_TOKENS, 4096);
        assert_eq!(ARBITRATE_MAX_TOKENS, 8192);
    }

    #[test]
    fn normalize_fills_zero_fields() {
        let c = AgentLoopConfig {
            max_steps: 0,
            bash_timeout: Duration::ZERO,
            ..Default::default()
        }
        .normalized();
        assert_eq!(c.max_steps, 40);
        assert_eq!(c.bash_timeout, Duration::from_secs(60));
    }

    #[test]
    fn normalize_keeps_explicit_values() {
        let c = AgentLoopConfig {
            max_steps: 5,
            bash_timeout: Duration::from_secs(1),
            ..Default::default()
        }
        .normalized();
        assert_eq!(c.max_steps, 5);
        assert_eq!(c.bash_timeout, Duration::from_secs(1));
    }

    #[test]
    fn or_no_reply_matches_go() {
        assert_eq!(or_no_reply(""), "(no reply)");
        assert_eq!(or_no_reply("   "), "(no reply)");
        assert_eq!(or_no_reply("done"), "done");
    }

    #[test]
    fn usage_accumulates() {
        let mut u = Usage::default();
        u.add(&Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        });
        u.add(&Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        });
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 18);
    }
}
