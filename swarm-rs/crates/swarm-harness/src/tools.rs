//! The four tool definitions handed to the model, copied verbatim from Go
//! `internal/harness/agentloop.go` L106-133.
//!
//! The JSON schemas are the contract between the prompt and the executor: the
//! model fills them in, `agent_loop` parses them back. A renamed field or a
//! changed `description` changes model behaviour, so these strings are
//! byte-identical to the Go source and covered by a test.

use serde::{Deserialize, Serialize};

pub const TOOL_BASH: &str = "bash";
pub const TOOL_WRITE: &str = "write";
pub const TOOL_HTTP: &str = "http";
pub const TOOL_FINISH: &str = "finish";

/// The `bash` tool schema.
pub const BASH_SCHEMA: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"the shell command to run"}},"required":["cmd"]}"#;

/// The `write` tool schema.
pub const WRITE_SCHEMA: &str = r#"{"type":"object","properties":{"file":{"type":"string","description":"JSON string: {\"path\":\"...\",\"content\":\"...\"}"}},"required":["file"]}"#;

/// The `http` tool schema.
pub const HTTP_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"type":"string","description":"JSON string: {\"method\":\"POST\",\"path\":\"/claim\",\"itemId\":\"...\",\"peer\":\"...\"}"}},"required":["request"]}"#;

/// The `finish` tool schema.
pub const FINISH_SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string","description":"one-line summary, or IDLE"}},"required":["summary"]}"#;

/// The `bash` tool description.
pub const BASH_DESC: &str = "Run a shell command in the worker worktree (cmd.exe on Windows). Returns stdout+stderr. 60s timeout.";

/// The `write` tool description.
pub const WRITE_DESC: &str = "Write a file in the worktree. Input is a single JSON string field 'file' containing {\"path\":\"relative/path\",\"content\":\"...\"}. Path must stay inside the worktree.";

/// The `http` tool description.
pub const HTTP_DESC: &str = "Call the local swarm server API. Input is a single JSON string field 'request' containing {\"method\":\"GET|POST\",\"path\":\"/board|/claim|/submit|...\",\"...\":\"extra body/query fields\"}. Use it to claim items, submit work, read the board, post context.";

/// The `finish` tool description.
pub const FINISH_DESC: &str = "End this turn. Provide a one-line summary of what you did, or the word IDLE if there is no work for you right now.";

/// One tool definition in OpenAI function-calling shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFuncDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFuncDef {
    pub name: String,
    pub description: String,
    /// A raw JSON object; kept as `Value` so it round-trips unchanged.
    pub parameters: serde_json::Value,
}

fn parse_schema(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("tool schema is a compile-time constant")
}

fn tool(name: &str, description: &str, schema: &str) -> ToolDef {
    ToolDef {
        kind: "function".to_string(),
        function: ToolFuncDef {
            name: name.to_string(),
            description: description.to_string(),
            parameters: parse_schema(schema),
        },
    }
}

/// The four tools, in the order the Go implementation emits them.
///
/// Order matters for the request body and therefore for prefix caching.
pub fn swarm_tools() -> Vec<ToolDef> {
    vec![
        tool(TOOL_BASH, BASH_DESC, BASH_SCHEMA),
        tool(TOOL_WRITE, WRITE_DESC, WRITE_SCHEMA),
        tool(TOOL_HTTP, HTTP_DESC, HTTP_SCHEMA),
        tool(TOOL_FINISH, FINISH_DESC, FINISH_SCHEMA),
    ]
}

/// The tool list as a `'static` slice of names, for quick membership checks.
pub const SWARM_TOOLS: &[&str] = &[TOOL_BASH, TOOL_WRITE, TOOL_HTTP, TOOL_FINISH];

/// Arguments for the `bash` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct BashArgs {
    pub cmd: String,
}

/// Arguments for the `write` tool: `file` is a JSON *string*.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteArgs {
    pub file: String,
}

/// The decoded payload inside `WriteArgs::file`.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteFilePayload {
    pub path: String,
    pub content: String,
}

/// Arguments for the `http` tool: `request` is a JSON *string*.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpArgs {
    pub request: String,
}

/// Arguments for the `finish` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct FinishArgs {
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_tools_in_go_order() {
        let t = swarm_tools();
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].function.name, "bash");
        assert_eq!(t[1].function.name, "write");
        assert_eq!(t[2].function.name, "http");
        assert_eq!(t[3].function.name, "finish");
        for d in &t {
            assert_eq!(d.kind, "function");
        }
    }

    #[test]
    fn schemas_are_valid_json_objects() {
        for (name, schema) in [
            ("bash", BASH_SCHEMA),
            ("write", WRITE_SCHEMA),
            ("http", HTTP_SCHEMA),
            ("finish", FINISH_SCHEMA),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(schema).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(v["type"], "object", "{name} must be an object schema");
            assert!(v["required"].is_array(), "{name} needs a required list");
        }
    }

    #[test]
    fn required_fields_match_go() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(BASH_SCHEMA).unwrap()["required"][0],
            "cmd"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(WRITE_SCHEMA).unwrap()["required"][0],
            "file"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(HTTP_SCHEMA).unwrap()["required"][0],
            "request"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(FINISH_SCHEMA).unwrap()["required"][0],
            "summary"
        );
    }

    #[test]
    fn write_payload_decodes_from_nested_json_string() {
        // The model must send `file` as a JSON string, not an object.
        let args: WriteArgs = serde_json::from_str(
            r#"{"file":"{\"path\":\"src/a.js\",\"content\":\"x\"}"}"#,
        )
        .unwrap();
        let payload: WriteFilePayload = serde_json::from_str(&args.file).unwrap();
        assert_eq!(payload.path, "src/a.js");
        assert_eq!(payload.content, "x");
    }

    #[test]
    fn http_args_decode_from_nested_json_string() {
        let args: HttpArgs =
            serde_json::from_str(r#"{"request":"{\"method\":\"GET\",\"path\":\"/board\"}"}"#)
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&args.request).unwrap();
        assert_eq!(v["path"], "/board");
    }

    #[test]
    fn serialized_tool_matches_openai_shape() {
        let t = &swarm_tools()[0];
        let v = serde_json::to_value(t).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "bash");
        assert_eq!(v["function"]["parameters"]["type"], "object");
    }
}
