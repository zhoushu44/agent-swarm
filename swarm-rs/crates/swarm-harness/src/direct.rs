//! The `direct` harness: an OpenAI-compatible HTTP chat client.
//!
//! Mirrors Go `internal/harness/direct.go`. Kept deliberately thin — it builds
//! a request, retries transport failures with linear backoff, and hands back the
//! decoded body. All agent behaviour lives in `agent_loop`.

use std::time::Duration;

use crate::agent_loop::{BoxFuture, ChatClient, ChatRequest, ChatResponse, HarnessError};

/// Default retry count for transport-level failures.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default backoff between retries.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Default chat path appended to the base URL.
pub const DEFAULT_CHAT_PATH: &str = "/chat/completions";

/// Configuration for the direct harness.
#[derive(Debug, Clone)]
pub struct DirectConfig {
    /// Base URL, e.g. `https://api.deepseek.com/v1`.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Appended to `base_url`. Defaults to `/chat/completions`.
    pub chat_path: String,
    pub max_retries: u32,
    pub retry_backoff: Duration,
}

impl Default for DirectConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            chat_path: DEFAULT_CHAT_PATH.to_string(),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_backoff: DEFAULT_RETRY_BACKOFF,
        }
    }
}

impl DirectConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    /// The full chat-completions URL.
    pub fn url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.chat_path
        )
    }
}

/// An OpenAI-compatible chat client.
#[derive(Debug)]
pub struct DirectHarness {
    cfg: DirectConfig,
    client: reqwest::Client,
    /// HTTP status of the most recent non-success response, if any.
    ///
    /// The loop's return type is shared with the echo harness, which never
    /// touches the network, so this is a side channel rather than a field on
    /// the result. It lets the caller tell a 429 (back off) from a 400 (stop
    /// retrying) instead of guessing from "no steps happened".
    last_status: std::sync::Mutex<Option<u16>>,
}

impl DirectHarness {
    pub fn new(cfg: DirectConfig) -> Result<Self, HarnessError> {
        if cfg.base_url.trim().is_empty() {
            return Err(HarnessError::Transport("empty base_url".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| HarnessError::Transport(e.to_string()))?;
        Ok(Self {
            cfg,
            client,
            last_status: std::sync::Mutex::new(None),
        })
    }

    pub fn config(&self) -> &DirectConfig {
        &self.cfg
    }

    /// HTTP status of the last non-success response, or `None` if every request
    /// so far succeeded.
    pub fn last_http_status(&self) -> Option<u16> {
        self.last_status.lock().ok().and_then(|s| *s)
    }

    /// Record the status of a non-success response.
    fn note_status(&self, code: u16) {
        if let Ok(mut s) = self.last_status.lock() {
            *s = Some(code);
        }
    }

    /// One round-trip with linear backoff on transport errors.
    async fn round_trip(&self, req: &ChatRequest) -> Result<ChatResponse, HarnessError> {
        let url = self.cfg.url();
        let body = serde_json::to_vec(req).map_err(|e| HarnessError::Encode(e.to_string()))?;

        let mut last_err = HarnessError::Transport("no attempt made".into());
        for attempt in 0..=self.cfg.max_retries {
            if attempt > 0 {
                tokio::time::sleep(self.cfg.retry_backoff * attempt).await;
            }
            let mut builder = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(body.clone());
            if !self.cfg.api_key.is_empty() {
                builder = builder.header("Authorization", format!("Bearer {}", self.cfg.api_key));
            }

            let resp = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = HarnessError::Transport(e.to_string());
                    continue;
                }
            };

            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                let code = status.as_u16();
                self.note_status(code);
                // 4xx (except 429) will not get better by retrying.
                if code != 429 && (400..500).contains(&code) {
                    return Err(HarnessError::Http { status: code, body: text });
                }
                last_err = HarnessError::Http { status: code, body: text };
                continue;
            }

            return serde_json::from_str::<ChatResponse>(&text).map_err(|e| {
                HarnessError::Transport(format!("decode response: {} (body: {})", e, truncate(&text, 300)))
            });
        }
        Err(last_err)
    }
}

impl ChatClient for DirectHarness {
    fn chat<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatResponse, HarnessError>> {
        Box::pin(async move { self.round_trip(&req).await })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Used only by the 429-retry tests, which count the requests a fake endpoint
    // actually served. Scoped to the test module so the library build stays
    // warning-free.
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn url_joins_without_double_slash() {
        let c = DirectConfig::new("https://api.example.com/v1/", "k", "m");
        assert_eq!(c.url(), "https://api.example.com/v1/chat/completions");
        let c2 = DirectConfig::new("https://api.example.com/v1", "k", "m");
        assert_eq!(c2.url(), "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn defaults_match_go() {
        let c = DirectConfig::default();
        assert_eq!(c.chat_path, "/chat/completions");
        assert_eq!(c.max_retries, 2);
        assert_eq!(c.retry_backoff, Duration::from_secs(1));
    }

    #[test]
    fn empty_base_url_is_rejected() {
        let err = DirectHarness::new(DirectConfig::default()).unwrap_err();
        assert!(matches!(err, HarnessError::Transport(_)));
    }

    #[tokio::test]
    async fn decodes_a_chat_response() {
        // A one-shot local server returning a minimal OpenAI body.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                let body = r#"{"choices":[{"message":{"content":"hi","tool_calls":[]}}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let cfg = DirectConfig::new(format!("http://127.0.0.1:{}", port), "k", "m");
        let h = DirectHarness::new(cfg).unwrap();
        let resp = h
            .chat(ChatRequest::new("m", vec![]))
            .await
            .expect("chat should succeed");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hi"));
        assert_eq!(resp.usage.total_tokens, 5);
    }

    #[tokio::test]
    async fn non_retryable_4xx_returns_http_error() {        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope")
                    .await;
                let _ = sock.shutdown().await;
            }
        });

        let cfg = DirectConfig::new(format!("http://127.0.0.1:{}", port), "k", "m");
        let h = DirectHarness::new(cfg).unwrap();
        let err = h.chat(ChatRequest::new("m", vec![])).await.unwrap_err();
        assert_eq!(err.status(), Some(400), "4xx must surface as an HTTP error");
    }

    /// A 429 must be RETRIED, not returned immediately.
    ///
    /// A throttle is transient by definition — the right response is to wait and
    /// try again, which is the opposite of a 400. This distinction is what lets
    /// the AIMD gate see a *sustained* throttle (retries exhausted) rather than
    /// every individual blip, and it is why the gate's back-off is measured in
    /// whole seconds while the client's retry is measured in one.
    #[tokio::test]
    async fn a_429_is_retried_before_surfacing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_server = attempts.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let n = attempts_for_server.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    // First two attempts: throttled.
                    let body = r#"{"error":{"message":"slow down"}}"#;
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await;
                } else {
                    // Third attempt: success, proving the retry loop worked.
                    let body = r#"{"choices":[{"message":{"content":"ok","tool_calls":[]}}],"usage":{"total_tokens":3}}"#;
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await;
                }
                let _ = sock.shutdown().await;
            }
        });

        let cfg = DirectConfig::new(format!("http://127.0.0.1:{}", port), "k", "m");
        let h = DirectHarness::new(cfg).unwrap();
        let resp = h.chat(ChatRequest::new("m", vec![])).await;
        assert!(resp.is_ok(), "a transient 429 must be retried into success");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "two 429s then a success — the retry loop must have run"
        );
        // After a success the status side-channel is stale-but-harmless; it is
        // only read when the call fails.
        let _ = h.last_http_status();
    }

    /// A 429 that never clears must surface as an HTTP error carrying 429, so the
    /// caller can tell "throttled" from "malformed request".
    #[tokio::test]
    async fn a_persistent_429_surfaces_with_its_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"error":{"message":"rate limited"}}"#;
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });

        let cfg = DirectConfig::new(format!("http://127.0.0.1:{}", port), "k", "m");
        let h = DirectHarness::new(cfg).unwrap();
        let err = h.chat(ChatRequest::new("m", vec![])).await.unwrap_err();
        assert_eq!(
            err.status(),
            Some(429),
            "an unrelenting throttle must surface as 429, not a generic error"
        );
        assert_eq!(
            h.last_http_status(),
            Some(429),
            "the side channel must agree with the error"
        );
    }
}

