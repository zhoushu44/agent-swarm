//! The 4-tool agent loop, mirroring Go `internal/harness/agentloop.go`.
//!
//! One worker turn is: send the prompt + 4 tool definitions to an
//! OpenAI-compatible endpoint with `tool_choice: "required"`, execute whatever
//! tools the model asks for, feed the results back, repeat. The loop ends when
//! the model calls `finish`, replies in plain text *after* submitting, or hits
//! `max_steps`.
//!
//! Two behaviours here are load-bearing and were each added after a real
//! failure:
//!
//! 1. **Re-prime on plain text.** Some endpoints ignore `tool_choice:
//!    "required"` and answer in prose. Treating that as completion makes the
//!    worker exit having done nothing, so the loop re-primes up to
//!    `MAX_PLAIN_TEXT_RETRIES` times — unless something was already submitted,
//!    in which case a plain-text wrap-up is a legitimate finish.
//! 2. **Parallel tool calls.** Models batch independent operations ("write 3
//!    files"), and running them concurrently collapses N round-trips into one.
//!    Results are written back in the ORIGINAL call order so the transcript
//!    stays deterministic for the model.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::bash_guard::check_bash_command;
use crate::tools::{swarm_tools, ToolDef};
use crate::{AgentLoopConfig, AgentLoopResult, Usage, LOOP_MAX_TOKENS, MAX_PLAIN_TEXT_RETRIES, REPRIME_MESSAGE};

/// Bash output is truncated to this many bytes before going back to the model.
pub const BASH_OUTPUT_LIMIT: usize = 16_000;

/// HTTP tool output is truncated to this many bytes.
pub const HTTP_OUTPUT_LIMIT: usize = 8_000;

/// The HTTP tool reads at most this much of the response body.
pub const HTTP_BODY_LIMIT: u64 = 1 << 20;

/// Timeout for one HTTP tool call.
pub const HTTP_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// A message in the agent-loop transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl LoopMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            name: String::new(),
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls,
            tool_call_id: String::new(),
            name: String::new(),
        }
    }

    pub fn tool(id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: id.into(),
            name: name.into(),
        }
    }
}

/// One tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

fn function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Raw JSON string, as OpenAI sends it.
    #[serde(default)]
    pub arguments: String,
}

impl ToolCall {
    /// Decode `arguments` into a JSON object, defaulting to empty on failure
    /// (Go does the same: a malformed argument blob becomes `{}`).
    pub fn args(&self) -> serde_json::Map<String, Value> {
        match serde_json::from_str::<Value>(&self.function.arguments) {
            Ok(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        }
    }

    /// Read a string field from the arguments.
    pub fn arg_str(&self, key: &str) -> String {
        self.args()
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Request body for one loop round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<LoopMessage>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: String,
    pub max_tokens: u32,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<LoopMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: swarm_tools(),
            tool_choice: "required".to_string(),
            max_tokens: LOOP_MAX_TOKENS,
        }
    }
}

/// Response body for one loop round-trip (only the fields the loop uses).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: WireUsage,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Choice {
    #[serde(default)]
    pub message: ChoiceMessage,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChoiceMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
}

impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }
    }
}

/// Errors the loop can return.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness: empty completion (no choices)")]
    EmptyCompletion,
    #[error("harness: no model configured")]
    NoModel,
    #[error("harness: transport: {0}")]
    Transport(String),
    #[error("harness: encode request: {0}")]
    Encode(String),
    #[error("harness: http {status}: {body}")]
    Http { status: u16, body: String },
}

impl HarnessError {
    /// The HTTP status, when this error came from a non-2xx response.
    pub fn status(&self) -> Option<u16> {
        match self {
            HarnessError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Anything that can answer one chat round-trip. Implemented by the real HTTP
/// client and by test doubles.
///
/// Object-safe via a boxed future, so `run_agent_loop` can take `&dyn` and
/// tests can inject a scripted fake.
pub trait ChatClient: Send + Sync {
    fn chat<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatResponse, HarnessError>>;
}

/// A boxed future, for the object-safe client trait.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Run the agent loop for one turn.
pub async fn run_agent_loop<C: ChatClient + ?Sized>(
    client: &C,
    model: &str,
    prompt: &str,
    cfg: &AgentLoopConfig,
) -> Result<AgentLoopResult, HarnessError> {
    if model.trim().is_empty() {
        return Err(HarnessError::NoModel);
    }
    let cfg = cfg.clone().normalized();

    let mut messages = vec![LoopMessage::user(prompt)];
    let mut total = Usage::default();
    let mut last_text = String::new();
    let submitted = std::sync::Arc::new(Mutex::new(false));
    let mut plain_text_retries = 0u32;

    for step in 0..cfg.max_steps {
        let req = ChatRequest::new(model, messages.clone());
        let resp = client.chat(req).await?;

        total.add(&resp.usage.into());

        let choice = match resp.choices.first() {
            Some(c) => c,
            None => return Err(HarnessError::EmptyCompletion),
        };
        let msg = &choice.message;
        if let Some(c) = &msg.content {
            if !c.is_empty() {
                last_text = c.clone();
            }
        }

        // Append the assistant turn (with tool calls when present).
        messages.push(LoopMessage::assistant(
            msg.content.clone().unwrap_or_default(),
            msg.tool_calls.clone(),
        ));

        if msg.tool_calls.is_empty() {
            // Plain-text reply with no tool call.
            let already_submitted = *submitted.lock().await;
            if already_submitted {
                // Work is with the integrator; a trailing wrap-up is a
                // legitimate finish.
                return Ok(AgentLoopResult {
                    reply: crate::or_no_reply(&last_text),
                    usage: total,
                    steps: step + 1,
                    submitted: true,
                });
            }
            plain_text_retries += 1;
            if plain_text_retries > MAX_PLAIN_TEXT_RETRIES {
                // The model keeps refusing tools; give up rather than burn
                // tokens.
                return Ok(AgentLoopResult {
                    reply: crate::or_no_reply(&last_text),
                    usage: total,
                    steps: step + 1,
                    submitted: false,
                });
            }
            messages.push(LoopMessage::user(REPRIME_MESSAGE));
            continue;
        }

        // Execute the tool calls. `finish` short-circuits; the rest run
        // concurrently but are written back in the original order.
        let mut finished = false;
        let mut finish_summary = String::new();
        let mut outputs: Vec<(String, String, String)> = Vec::with_capacity(msg.tool_calls.len());
        let mut to_run: Vec<(usize, ToolCall)> = Vec::new();

        for (i, tc) in msg.tool_calls.iter().enumerate() {
            if tc.function.name == crate::TOOL_FINISH {
                finished = true;
                finish_summary = tc.arg_str("summary");
                outputs.push((tc.id.clone(), tc.function.name.clone(), "turn ended".to_string()));
            } else {
                to_run.push((i, tc.clone()));
            }
        }

        // Fan out the remaining tools.
        let mut handles = Vec::with_capacity(to_run.len());
        for (i, tc) in to_run {
            let cfg = cfg.clone();
            let submitted = submitted.clone();
            handles.push((
                i,
                tc.id.clone(),
                tc.function.name.clone(),
                tokio::spawn(async move {
                    exec_tool(&tc.function.name, &tc, &cfg, submitted).await
                }),
            ));
        }
        let mut results: Vec<(usize, String, String, String)> = Vec::with_capacity(handles.len());
        for (i, id, name, h) in handles {
            let out = h.await.unwrap_or_else(|e| format!("tool panicked: {e}"));
            results.push((i, id, name, out));
        }
        // Restore the model's original ordering.
        results.sort_by_key(|(i, _, _, _)| *i);
        for (_, id, name, out) in results {
            outputs.push((id, name, out));
        }

        for (id, name, out) in outputs {
            messages.push(LoopMessage::tool(id, name, out));
        }

        if finished {
            let reply = if finish_summary.is_empty() {
                last_text.clone()
            } else {
                finish_summary
            };
            let submitted_now = *submitted.lock().await;
            return Ok(AgentLoopResult {
                reply: crate::or_no_reply(&reply),
                usage: total,
                steps: step + 1,
                submitted: submitted_now,
            });
        }
    }

    let submitted_now = *submitted.lock().await;
    Ok(AgentLoopResult {
        reply: format!("{} (max steps)", crate::or_no_reply(&last_text)),
        usage: total,
        steps: cfg.max_steps,
        submitted: submitted_now,
    })
}

/// Dispatch one tool call.
async fn exec_tool(
    name: &str,
    tc: &ToolCall,
    cfg: &AgentLoopConfig,
    submitted: std::sync::Arc<Mutex<bool>>,
) -> String {
    match name {
        crate::TOOL_BASH => {
            let cmd = tc.arg_str("cmd");
            run_bash_tool(&cmd, &cfg.work_dir, cfg.bash_timeout).await
        }
        crate::TOOL_WRITE => {
            let file = tc.arg_str("file");
            run_write_tool(&file, &cfg.work_dir)
        }
        crate::TOOL_HTTP => {
            let request = tc.arg_str("request");
            run_http_tool(&request, cfg.server_port, submitted).await
        }
        other => format!("unknown tool: {}", other),
    }
}

/// Run a shell command in `dir`, bounded by `timeout`.
///
/// The guard runs first: a prompt rule asking for the `write` tool instead is
/// advisory and the model ignored it repeatedly; this is not.
pub async fn run_bash_tool(cmd: &str, dir: &str, timeout: Duration) -> String {
    if cmd.trim().is_empty() {
        return "(empty command)".to_string();
    }
    let verdict = check_bash_command(cmd);
    if verdict.blocked {
        return format!("bash refused: {}. {}", verdict.reason, verdict.hint);
    }

    // NOTE: the Go implementation hardcodes `cmd /c`, i.e. it only works on
    // Windows. We pick the platform shell so the same code runs on Linux CI;
    // see docs/OPTIMIZATION-SWARM.md §3.6.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };

    let mut command = tokio::process::Command::new(shell);
    command.arg(flag).arg(cmd);
    if !dir.is_empty() {
        command.current_dir(dir);
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let fut = command.output();
    let out = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return format!("exit ?:\n{}", e),
        Err(_) => {
            // The child is killed when the future is dropped by `output()`'s
            // internal kill_on_drop is off, so report the timeout explicitly.
            return "exit ? (TIMEOUT/KILLED):\ncommand timed out".to_string();
        }
    };

    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if text.len() > BASH_OUTPUT_LIMIT {
        // See the note in `run_http_tool`: `truncate` cuts by BYTE and panics on
        // a multi-byte boundary. Bash output containing any non-ASCII prose
        // reaches that case quickly.
        truncate_chars(&mut text, BASH_OUTPUT_LIMIT);
        text.push_str("\n...(truncated)");
    }

    if out.status.success() {
        if text.is_empty() {
            return "(no output)".to_string();
        }
        return text;
    }
    let code = out
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    if text.is_empty() {
        text = format!("exit status: {:?}", out.status);
    }
    format!("exit {}:\n{}", code, text)
}

/// Write `{path, content}` JSON under `dir`, rejecting paths that escape it.
pub fn run_write_tool(file_json: &str, dir: &str) -> String {
    if file_json.trim().is_empty() {
        return "write failed: empty file payload".to_string();
    }
    let payload: crate::tools::WriteFilePayload = match serde_json::from_str(file_json) {
        Ok(p) => p,
        Err(e) => return format!("write failed: bad JSON: {}", e),
    };
    if payload.path.is_empty() {
        return "write failed: missing path".to_string();
    }

    let clean = normalize_rel_path(&payload.path);
    let clean = match clean {
        Some(p) => p,
        None => return format!("write failed: path escapes worktree: {}", payload.path),
    };

    let abs = if dir.is_empty() {
        clean.clone()
    } else {
        PathBuf::from(dir).join(&clean)
    };
    if let Some(parent) = abs.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("write failed: {}", e);
            }
        }
    }
    if let Err(e) = std::fs::write(&abs, payload.content.as_bytes()) {
        return format!("write failed: {}", e);
    }
    let lines = payload.content.matches('\n').count() + 1;
    format!(
        "wrote {} ({} bytes, {} lines)",
        payload.path,
        payload.content.len(),
        lines
    )
}

/// Normalize a relative path, returning `None` when it escapes the worktree.
///
/// Rejects absolute paths and any `..` component, matching Go's
/// `strings.HasPrefix(clean, "..") || filepath.IsAbs(clean)`.
pub fn normalize_rel_path(p: &str) -> Option<PathBuf> {
    let raw = Path::new(p);
    if raw.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// Call the local swarm server. `request` is JSON `{method, path, ...fields}`.
///
/// GET fields become query params; POST fields become the JSON body.
pub async fn run_http_tool(
    request_json: &str,
    port: u16,
    submitted: std::sync::Arc<Mutex<bool>>,
) -> String {
    let (method, mut path, fields) = normalize_http_input(request_json);
    if path.is_empty() {
        return "http failed: missing path".to_string();
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    let base = format!("http://127.0.0.1:{}", port);

    let client = match reqwest::Client::builder()
        .timeout(HTTP_TOOL_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("http failed: {}", e),
    };

    let resp = if method == "GET" {
        let mut query: Vec<(String, String)> = Vec::new();
        for (k, v) in &fields {
            query.push((k.clone(), json_scalar_to_string(v)));
        }
        match client.get(format!("{}{}", base, path)).query(&query).send().await {
            Ok(r) => r,
            Err(e) => return format!("http failed: {}", e),
        }
    } else {
        match client
            .post(format!("{}{}", base, path))
            .json(&fields)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("http failed: {}", e),
        }
    };

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let mut text = body;
    if text.len() > HTTP_OUTPUT_LIMIT {
        // Truncate on a char boundary, not a byte one. `String::truncate`
        // panics when the cut lands inside a multi-byte UTF-8 sequence, and an
        // endpoint that returns Chinese, emoji, or any non-ASCII prose cuts
        // there sooner or later — the M4 benchmark hit it on the first long
        // response.
        truncate_chars(&mut text, HTTP_OUTPUT_LIMIT);
        text.push_str("...(truncated)");
    }
    let code = status.as_u16();

    if method == "POST" && path == "/submit" && (200..300).contains(&code) {
        *submitted.lock().await = true;
    }

    format!("{} {}", code, text)
}

/// Truncate `s` to at most `max_bytes`, never splitting a UTF-8 char.
///
/// `String::truncate` takes a BYTE index and panics if it lands inside a
/// multi-byte sequence. Every caller here is truncating model output, command
/// output, or a file read — all of which routinely contain non-ASCII text — so
/// the byte cut is a latent panic rather than a theoretical one. The M4
/// benchmark found it on the first long Chinese response.
///
/// The result may be a few bytes shorter than `max_bytes`; that is the point.
fn truncate_chars(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Render a JSON value the way Go's `fmt.Sprintf("%v", v)` would for the common
/// scalar cases (strings verbatim, numbers/bools via their JSON form).
fn json_scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parse the `http` tool input, accepting the single-string `request` JSON plus
/// legacy flat shapes.
///
/// Returns `(method, path, fields)`. `method` defaults to GET, and
/// `method`/`path` are excluded from `fields`.
pub fn normalize_http_input(request_json: &str) -> (String, String, serde_json::Map<String, Value>) {
    let mut method = "GET".to_string();
    let mut path = String::new();
    let mut fields = serde_json::Map::new();

    let parsed: Value = match serde_json::from_str(request_json) {
        Ok(v) => v,
        Err(_) => return (method, path, fields),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return (method, path, fields),
    };

    if let Some(m) = obj.get("method").and_then(|v| v.as_str()) {
        if !m.is_empty() {
            method = m.to_uppercase();
        }
    }
    if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
        path = p.to_string();
    }
    for (k, v) in obj {
        if k == "method" || k == "path" {
            continue;
        }
        if v.is_null() {
            continue;
        }
        if let Some(s) = v.as_str() {
            if s.is_empty() {
                continue;
            }
        }
        fields.insert(k.clone(), v.clone());
    }
    (method, path, fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_rejects_parent_escape() {
        let out = run_write_tool(r#"{"path":"../etc/passwd","content":"x"}"#, ".");
        assert!(out.contains("escapes worktree"), "got: {out}");
    }

    #[test]
    fn write_rejects_absolute_path() {
        let out = run_write_tool(r#"{"path":"/etc/passwd","content":"x"}"#, ".");
        assert!(out.contains("escapes worktree"), "got: {out}");
    }

    #[test]
    fn write_accepts_nested_relative_path() {
        let tmp = std::env::temp_dir().join(format!("swarm-write-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let out = run_write_tool(
            r#"{"path":"src/deep/a.js","content":"line1\nline2"}"#,
            tmp.to_str().unwrap(),
        );
        assert!(out.starts_with("wrote src/deep/a.js"), "got: {out}");
        assert!(out.contains("2 lines"), "got: {out}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_reports_bad_json() {
        let out = run_write_tool("not json", ".");
        assert!(out.contains("bad JSON"));
    }

    #[test]
    fn write_reports_empty_payload() {
        assert!(run_write_tool("  ", ".").contains("empty file payload"));
    }

    #[test]
    fn normalize_rel_path_handles_dot_segments() {
        assert_eq!(
            normalize_rel_path("./src/a.js"),
            Some(PathBuf::from("src").join("a.js"))
        );
        assert_eq!(normalize_rel_path("../x"), None);
        assert_eq!(normalize_rel_path(""), None);
    }

    #[tokio::test]
    async fn bash_guard_runs_before_exec() {
        let out = run_bash_tool("node tmp_patch.mjs", ".", Duration::from_secs(5)).await;
        assert!(out.starts_with("bash refused:"), "got: {out}");
    }

    #[tokio::test]
    async fn bash_empty_command() {
        let out = run_bash_tool("   ", ".", Duration::from_secs(5)).await;
        assert_eq!(out, "(empty command)");
    }

    #[tokio::test]
    async fn bash_captures_output() {
        if cfg!(windows) {
            let out = run_bash_tool("echo hello", ".", Duration::from_secs(10)).await;
            assert!(out.contains("hello"), "got: {out}");
        } else {
            let out = run_bash_tool("echo hello", ".", Duration::from_secs(10)).await;
            assert!(out.contains("hello"), "got: {out}");
        }
    }

    #[tokio::test]
    async fn bash_reports_nonzero_exit() {
        let cmd = if cfg!(windows) { "exit /b 3" } else { "exit 3" };
        let out = run_bash_tool(cmd, ".", Duration::from_secs(10)).await;
        assert!(out.starts_with("exit 3:"), "got: {out}");
    }

    #[test]
    fn normalize_http_input_defaults_to_get() {
        let (m, p, f) = normalize_http_input(r#"{"path":"/board"}"#);
        assert_eq!(m, "GET");
        assert_eq!(p, "/board");
        assert!(f.is_empty());
    }

    #[test]
    fn normalize_http_input_uppercases_method() {
        let (m, _, _) = normalize_http_input(r#"{"method":"post","path":"/claim"}"#);
        assert_eq!(m, "POST");
    }

    #[test]
    fn normalize_http_input_drops_empty_and_null() {
        let (_, _, f) = normalize_http_input(
            r#"{"path":"/x","empty":"","nil":null,"keep":"v","num":7}"#,
        );
        assert!(!f.contains_key("empty"));
        assert!(!f.contains_key("nil"));
        assert_eq!(f.get("keep").unwrap(), "v");
        assert_eq!(f.get("num").unwrap(), 7);
    }

    #[test]
    fn normalize_http_input_tolerates_garbage() {
        let (m, p, f) = normalize_http_input("not json");
        assert_eq!(m, "GET");
        assert!(p.is_empty());
        assert!(f.is_empty());
    }

    #[test]
    fn tool_call_args_default_to_empty_on_bad_json() {
        let tc = ToolCall {
            id: "1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{broken".into(),
            },
        };
        assert!(tc.args().is_empty());
        assert_eq!(tc.arg_str("cmd"), "");
    }

    #[test]
    fn chat_request_uses_required_tool_choice() {
        let r = ChatRequest::new("m", vec![]);
        assert_eq!(r.tool_choice, "required");
        assert_eq!(r.max_tokens, LOOP_MAX_TOKENS);
        assert_eq!(r.tools.len(), 4);
    }
}
