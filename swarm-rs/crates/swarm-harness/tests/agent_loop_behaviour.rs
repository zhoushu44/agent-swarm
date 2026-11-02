//! Behavioural tests for the agent loop.
//!
//! These pin the two behaviours that were each added after a real failure:
//!
//! * **re-prime on plain text** — a model that answers in prose without calling
//!   a tool must be told to use one, up to 3 times, and must NOT be treated as
//!   finished unless something was already submitted;
//! * **parallel tool calls with ordered results** — batched independent calls
//!   run concurrently but are written back in the model's original order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use swarm_harness::AgentLoopConfig;
use swarm_harness::agent_loop::{
    run_agent_loop, BoxFuture, ChatClient, ChatRequest, ChatResponse, Choice,
    ChoiceMessage, HarnessError, ToolCall, ToolCallFunction, WireUsage,
};

/// A scripted client: returns one canned response per call, then repeats the
/// last one. Records the requests it saw.
struct ScriptedClient {
    responses: Mutex<Vec<ChatResponse>>,
    calls: AtomicUsize,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedClient {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl ChatClient for ScriptedClient {
    fn chat<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatResponse, HarnessError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(req);
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let list = self.responses.lock().unwrap();
            let idx = n.min(list.len().saturating_sub(1));
            Ok(list.get(idx).cloned().unwrap_or_default())
        })
    }
}

fn text_response(content: &str) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            message: ChoiceMessage {
                content: Some(content.to_string()),
                tool_calls: vec![],
            },
        }],
        usage: WireUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
    }
}

fn tool_response(calls: Vec<(&str, &str)>) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            message: ChoiceMessage {
                content: None,
                tool_calls: calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, args))| ToolCall {
                        id: format!("call_{}", i),
                        kind: "function".into(),
                        function: ToolCallFunction {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    })
                    .collect(),
            },
        }],
        usage: WireUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
    }
}

fn cfg(dir: &str) -> AgentLoopConfig {
    AgentLoopConfig {
        work_dir: dir.to_string(),
        server_port: 1, // nothing listens; http tool tests use their own server
        max_steps: 10,
        bash_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn finish_ends_the_loop() {
    let client = ScriptedClient::new(vec![tool_response(vec![("finish", r#"{"summary":"all done"}"#)])]);
    let r = run_agent_loop(&client, "m", "prompt", &cfg(".")).await.unwrap();
    assert_eq!(r.reply, "all done");
    assert_eq!(r.steps, 1);
    assert!(!r.submitted);
}

#[tokio::test]
async fn finish_with_empty_summary_falls_back_to_last_text() {
    let client = ScriptedClient::new(vec![
        ChatResponse {
            choices: vec![Choice {
                message: ChoiceMessage {
                    content: Some("thinking".into()),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        kind: "function".into(),
                        function: ToolCallFunction {
                            name: "finish".into(),
                            arguments: r#"{"summary":""}"#.into(),
                        },
                    }],
                },
            }],
            usage: WireUsage::default(),
        },
    ]);
    let r = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();
    assert_eq!(r.reply, "thinking");
}

/// The core re-prime behaviour: plain text with no tool call and nothing
/// submitted must NOT end the turn — the loop appends the re-prime message.
#[tokio::test]
async fn plain_text_without_submission_is_reprimed() {
    let client = ScriptedClient::new(vec![
        text_response("I think the answer is 42."),
        tool_response(vec![("finish", r#"{"summary":"done"}"#)]),
    ]);
    let r = run_agent_loop(&client, "m", "prompt", &cfg(".")).await.unwrap();

    assert_eq!(client.calls(), 2, "must call again after the re-prime");
    let reqs = client.requests();
    let second = &reqs[1];
    let last = second.messages.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(
        last.content.contains("You MUST call a tool now"),
        "re-prime text must be appended, got: {}",
        last.content
    );
    assert_eq!(r.reply, "done");
}

/// After 3 re-primes the loop gives up instead of burning tokens forever.
#[tokio::test]
async fn plain_text_gives_up_after_three_reprimes() {
    let client = ScriptedClient::new(vec![text_response("prose only")]);
    let r = run_agent_loop(&client, "m", "prompt", &cfg(".")).await.unwrap();

    // 1 initial + 3 re-primes = 4 calls, then it returns.
    assert_eq!(client.calls(), 4, "1 attempt + 3 re-primes");
    assert_eq!(r.reply, "prose only");
    assert!(!r.submitted);
}

/// A plain-text wrap-up AFTER a submit is a legitimate finish, not a re-prime.
#[tokio::test]
async fn plain_text_after_submit_finishes_without_reprime() {
    // Step 1: call http POST /submit against a tiny local server so `submitted`
    // becomes true. Step 2: reply in prose.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read until the request headers are complete, then answer.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match sock.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let client = ScriptedClient::new(vec![
        tool_response(vec![("http", r#"{"request":"{\"method\":\"POST\",\"path\":\"/submit\"}"}"#)]),
        text_response("submitted, wrapping up"),
    ]);
    let mut c = cfg(".");
    c.server_port = port;

    let r = run_agent_loop(&client, "m", "prompt", &c).await.unwrap();
    assert!(r.submitted, "submit must be recorded");
    assert_eq!(client.calls(), 2, "no re-prime after a submit");
    assert_eq!(r.reply, "submitted, wrapping up");
}

#[tokio::test]
async fn unknown_tool_reports_back_to_the_model() {
    let client = ScriptedClient::new(vec![
        tool_response(vec![("teleport", "{}")]),
        tool_response(vec![("finish", r#"{"summary":"ok"}"#)]),
    ]);
    let r = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();
    let reqs = client.requests();
    // The second request's transcript must contain the tool error.
    let transcript = format!("{:?}", reqs[1].messages);
    assert!(
        transcript.contains("unknown tool: teleport"),
        "tool error must be fed back"
    );
    assert_eq!(r.reply, "ok");
}

/// Parallel calls must be written back in the model's original order, even
/// though they execute concurrently.
#[tokio::test]
async fn parallel_tool_results_keep_original_order() {
    let client = ScriptedClient::new(vec![
        tool_response(vec![
            ("bash", r#"{"cmd":"echo first"}"#),
            ("bash", r#"{"cmd":"echo second"}"#),
            ("bash", r#"{"cmd":"echo third"}"#),
        ]),
        tool_response(vec![("finish", r#"{"summary":"done"}"#)]),
    ]);
    let _ = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();

    let reqs = client.requests();
    let msgs = &reqs[1].messages;
    let tool_msgs: Vec<_> = msgs.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tool_msgs.len(), 3);
    assert_eq!(tool_msgs[0].tool_call_id, "call_0");
    assert_eq!(tool_msgs[1].tool_call_id, "call_1");
    assert_eq!(tool_msgs[2].tool_call_id, "call_2");
    assert!(tool_msgs[0].content.contains("first"));
    assert!(tool_msgs[1].content.contains("second"));
    assert!(tool_msgs[2].content.contains("third"));
}

#[tokio::test]
async fn finish_short_circuits_other_calls() {
    // finish + a bash call in the same round: finish wins, bash still runs
    // (Go keeps its result as "turn ended" for finish and executes the rest).
    let client = ScriptedClient::new(vec![tool_response(vec![
        ("bash", r#"{"cmd":"echo ran"}"#),
        ("finish", r#"{"summary":"early"}"#),
    ])]);
    let r = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();
    assert_eq!(r.reply, "early");
    assert_eq!(client.calls(), 1, "finish must end the loop in one round");
}

#[tokio::test]
async fn max_steps_is_respected() {
    let client = ScriptedClient::new(vec![tool_response(vec![("bash", r#"{"cmd":"echo x"}"#)])]);
    let mut c = cfg(".");
    c.max_steps = 3;
    let r = run_agent_loop(&client, "m", "p", &c).await.unwrap();
    assert_eq!(client.calls(), 3);
    assert_eq!(r.steps, 3);
    assert!(r.reply.contains("max steps"), "got: {}", r.reply);
}

#[tokio::test]
async fn empty_choices_is_an_error() {
    let client = ScriptedClient::new(vec![ChatResponse::default()]);
    let err = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap_err();
    assert!(matches!(err, HarnessError::EmptyCompletion));
}

#[tokio::test]
async fn empty_model_is_rejected() {
    let client = ScriptedClient::new(vec![]);
    let err = run_agent_loop(&client, "  ", "p", &cfg(".")).await.unwrap_err();
    assert!(matches!(err, HarnessError::NoModel));
}

#[tokio::test]
async fn usage_accumulates_across_steps() {
    let client = ScriptedClient::new(vec![
        tool_response(vec![("bash", r#"{"cmd":"echo a"}"#)]),
        tool_response(vec![("bash", r#"{"cmd":"echo b"}"#)]),
        tool_response(vec![("finish", r#"{"summary":"ok"}"#)]),
    ]);
    let r = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();
    assert_eq!(r.steps, 3);
    assert_eq!(r.usage.prompt_tokens, 30, "3 steps x 10");
    assert_eq!(r.usage.completion_tokens, 15, "3 steps x 5");
    assert_eq!(r.usage.total_tokens, 45);
}

#[tokio::test]
async fn request_always_declares_four_tools() {
    let client = ScriptedClient::new(vec![tool_response(vec![("finish", r#"{"summary":"x"}"#)])]);
    let _ = run_agent_loop(&client, "m", "p", &cfg(".")).await.unwrap();
    let reqs = client.requests();
    assert_eq!(reqs[0].tools.len(), 4);
    assert_eq!(reqs[0].tool_choice, "required");
    assert_eq!(reqs[0].model, "m");
}
