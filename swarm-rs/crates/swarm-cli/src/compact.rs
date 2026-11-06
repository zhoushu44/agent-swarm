//! `swarm memory compact` — fold many notes into few.
//!
//! ## Why this is a command and not a swarm item
//!
//! A swarm item needs a board, a worktree, a claim, a gate and a merge. Compaction
//! needs none of those: it reads one table, calls the model once, writes rows back.
//! Routing it through the swarm would mean an item whose "worktree" is the user's
//! home directory and whose "acceptance test" is "does the prose read well" —
//! neither of which the gate can check. It would also contend on the memory file
//! with every worker that is concurrently writing FACTS.
//!
//! So: a direct, single-shot LLM call, run on demand.
//!
//! ## Why compaction is needed at all
//!
//! Every run appends every FACT it learned. Most of them are situational ("w3 is
//! analysing shard 046") and worthless a day later; a few are durable ("the
//! endpoint needs the sandbox flag"). Without folding, the prompt block grows
//! until the notes crowd out the task, and the recall in `render_memory_block`
//! starts dropping the useful ones because it is capped at 25.
//!
//! ## The contract
//!
//! Never loses a note silently. Every input note id must appear in exactly one
//! output cluster: either quoted into a merged note, or listed in `drop`. The
//! caller deletes `drop` only after verifying the coverage, so a model that
//! hallucinates half its input cannot erase the other half.

use std::collections::BTreeSet;

use swarm_store::{Memory, MemoryStore};
use swarm_harness::{ChatClient, ChatRequest, DirectConfig, DirectHarness, LoopMessage};

/// What the model is asked to return.
#[derive(Debug, Default)]
pub struct CompactPlan {
    /// Merged notes: (text, ids it replaces).
    pub merged: Vec<(String, Vec<i64>)>,
    /// Ids judged situational, to be deleted.
    pub drop: Vec<i64>,
    /// Ids from durable kinds (FAIL lessons) that must survive even if the model
    /// tried to drop them.
    pub kept: Vec<i64>,
}

/// Kinds that are never dropped, only merged.
///
/// A `FAIL` note is the record of a dead end that cost a real lease to discover.
/// Dropping one to save tokens is exactly the trade this whole feature exists to
/// prevent, so it is a hard rule rather than a prompt suggestion.
const DURABLE_KINDS: &[&str] = &["FAIL"];

const COMPACT_SYSTEM: &str = "\
Compress a memory file for an autonomous coding agent. You receive numbered notes \
collected across many runs. Call `emit_plan` exactly once.

WHAT TO DO
- Merge notes that teach the SAME lesson into ONE sentence. If two notes say the same \
thing about different runs, they are one note.
- DROP progress chatter outright: \"round N scanning\", \"worker wN is working on shard \
X\", \"starting analysis of Y\", \"wN online\". These describe a moment that has passed \
and are worthless tomorrow. Do not merge them into a sentence — drop them.
- Keep a note only if a future engineer reading it would act differently.

WRITING RULES (the output is read in isolation, months later)
- Never include the note id, the `#N` marker, or the bracket tag in the text. The text \
is the lesson, not a reference to it.
- Never start with \"#1\" or \"[FACT]\". Write the sentence itself.
- No pronouns pointing at another note (\"this\", \"it\", \"the above\"). No run ids, no \
peer names, no dates.
- Under 200 characters per merged text.

HARD CONSTRAINTS
- Every input id appears exactly once: inside some merged[].ids, or in drop.
- Notes whose kind is FAIL are hard-won dead ends: merge them, never drop them.";

/// The tool the model must call. A plain `return JSON` instruction did not work on
/// the endpoint this was developed against — the model answered with helpful prose
/// and suggestions instead, every time. Forcing a single function whose schema IS
/// the answer shape is the only reliable way to get structured output from a model
/// that has opinions.
fn compact_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "emit_plan",
            "description":
                "Emit the compaction plan. Every input id must appear exactly once: \
                 either inside some merged[].ids or in drop.",
            "parameters": {
                "type": "object",
                "properties": {
                    "merged": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "ids": { "type": "array", "items": { "type": "integer" } }
                            },
                            "required": ["text", "ids"]
                        }
                    },
                    "drop": { "type": "array", "items": { "type": "integer" } }
                },
                "required": ["merged", "drop"]
            }
        }
    })
}
/// Remove a leading `#12` or `[FACT]` marker from model output.
///
/// Repeated until neither pattern matches, because a model that echoes the input
/// format usually echoes both halves: `#3 [FACT] the lesson`. Cleaning here is
/// right — rejecting a whole plan over a formatting slip would make the command
/// unusable against models that do it inconsistently.
fn strip_markers(s: &str) -> String {
    let mut t = s.trim().to_string();
    loop {
        let before = t.clone();
        if let Some(rest) = t.strip_prefix('#') {
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            t = rest.trim_start().to_string();
        }
        if t.starts_with('[') {
            if let Some(close) = t.find(']') {
                t = t[close + 1..].trim_start().to_string();
            }
        }
        if t == before {
            break;
        }
    }
    t
}

/// Render notes as the numbered input the model sees.
fn render_notes(notes: &[Memory]) -> String {
    let mut out = String::new();
    for m in notes {
        let text = m.text.replace('\n', " ");
        out.push_str(&format!("#{} [{}] {}\n", m.id, m.kind, text));
    }
    out
}

/// Parse the model's JSON, tolerating a code fence and trailing prose.
///
/// Models routinely wrap JSON in ```json fences or add "Here is the result:".
/// Failing the whole compaction on that would make the command useless, so the
/// parser finds the first balanced object instead of demanding a clean body.
pub fn parse_plan(raw: &str) -> Result<CompactPlan, String> {
    let start = raw.find('{').ok_or("no JSON object in reply")?;
    let bytes = raw.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut end = None;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.ok_or("unbalanced JSON in reply")?;
    let slice = &raw[start..end];
    let v: serde_json::Value =
        serde_json::from_str(slice).map_err(|e| format!("reply is not JSON: {e}"))?;

    let mut plan = CompactPlan::default();
    if let Some(arr) = v.get("merged").and_then(|x| x.as_array()) {
        for item in arr {
            // Strip the rendered input format if the model echoed it back:
            // `[FACT] ...` and `#3 ...` are artefacts of how the notes were shown
            // to the model, not part of the lesson. Cleaning them here is right —
            // rejecting the whole plan over a formatting slip would make the
            // command unusable against models that do this inconsistently.
            let text = strip_markers(item.get("text").and_then(|x| x.as_str()).unwrap_or(""));
            if text.is_empty() {
                continue;
            }
            let ids: Vec<i64> = item
                .get("ids")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            plan.merged.push((text, ids));
        }
    }
    if let Some(arr) = v.get("drop").and_then(|x| x.as_array()) {
        plan.drop = arr.iter().filter_map(|x| x.as_i64()).collect();
    }
    Ok(plan)
}

/// Reject merged text that is just a copy of the input with its marker attached.
///
/// Observed on the first real run: the model returned
/// `"#1 [FACT] cmd.exe 里 git commit -m 带空格会截断"` verbatim, ids included, and
/// called it a merged note. That passes `verify_coverage` — the id is accounted
/// for — while producing memory that reads like a database dump. Cheaper to catch
/// here than to discover it after a month of accumulation.
fn looks_like_echo(text: &str, sources: &[&Memory]) -> bool {
    let t = text.trim();
    // A leading `[FACT]`/`#3` means the model copied the rendered input format
    // straight through. That is a formatting bug, but it is also a reliable sign
    // that no compression happened, so it counts as an echo on its own.
    if t.starts_with('#') || t.starts_with('[') {
        return true;
    }
    sources.iter().any(|m| {
        let src = m.text.trim();
        if src == t {
            return true;
        }
        // A "merge" whose only change is a short prefix or suffix around one of
        // its inputs (`"扫描进度快照：..." + source`) is still that input. Compare
        // after stripping a short lead-in, and only when the remainder is long
        // enough that the match cannot be coincidental.
        for prefix_len in [0usize, 8, 16, 24] {
            if let Some(rest) = t.get(prefix_len..) {
                if rest.len() > 24 && rest.starts_with(src) {
                    return true;
                }
            }
        }
        false
    })
}

/// Check that the plan accounts for every input exactly once.
///
/// This is the safety net that makes compaction safe to automate: a model that
/// silently forgets ten notes would otherwise delete them, and nobody would
/// notice until a run re-walked the same dead end.
pub fn verify_coverage(notes: &[Memory], plan: &CompactPlan) -> Result<(), String> {
    let input: BTreeSet<i64> = notes.iter().map(|m| m.id).collect();
    let mut seen: Vec<i64> = Vec::new();
    for (_, ids) in &plan.merged {
        seen.extend(ids.iter().copied());
    }
    seen.extend(plan.drop.iter().copied());

    let mut dupes: Vec<i64> = Vec::new();
    for id in &seen {
        if seen.iter().filter(|x| *x == id).count() > 1 {
            dupes.push(*id);
        }
    }
    dupes.sort_unstable();
    dupes.dedup();
    if !dupes.is_empty() {
        return Err(format!("ids appear more than once: {dupes:?}"));
    }

    let missing: Vec<i64> = input.difference(&seen.iter().copied().collect()).copied().collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} note(s) not accounted for: {missing:?} — refusing to apply, they would be lost",
            missing.len()
        ));
    }

    let extra: Vec<i64> = seen.iter().filter(|id| !input.contains(id)).copied().collect();
    if !extra.is_empty() {
        return Err(format!("ids not in the input: {extra:?}"));
    }

    // Durable kinds cannot be dropped, only merged.
    let durable: BTreeSet<i64> = notes
        .iter()
        .filter(|m| DURABLE_KINDS.contains(&m.kind.as_str()))
        .map(|m| m.id)
        .collect();
    let dropped_durable: Vec<i64> = plan
        .drop
        .iter()
        .filter(|id| durable.contains(id))
        .copied()
        .collect();
    if !dropped_durable.is_empty() {
        return Err(format!(
            "FAIL notes must be merged, not dropped: {dropped_durable:?}"
        ));
    }

    // Reject verbatim echoes: the first real run returned the inputs unchanged
    // with their `#N [FACT]` markers still attached.
    let by_id: std::collections::HashMap<i64, &Memory> =
        notes.iter().map(|m| (m.id, m)).collect();
    for (text, ids) in &plan.merged {
        // A single-source "merge" that copies its input is the right answer when
        // that note is already tight — there is nothing to compress, and forcing a
        // rewrite would only make it wordier. Only a MULTI-source cluster that
        // echoes one of its inputs has failed to do the job.
        if ids.len() < 2 {
            continue;
        }
        let sources: Vec<&Memory> = ids.iter().filter_map(|id| by_id.get(id).copied()).collect();
        if looks_like_echo(text, &sources) {
            return Err(format!(
                "merged note is a verbatim echo of one of its {} inputs: {text:?}",
                ids.len()
            ));
        }
    }

    Ok(())
}

/// Fold `notes` into fewer notes via one model call.
///
/// Returns the plan; applying it is the caller's job so a `--dry-run` can show
/// the result first.
pub fn plan_compaction(
    notes: &[Memory],
    api_base: &str,
    api_key: &str,
    model: &str,
) -> Result<(CompactPlan, String), String> {
    if notes.is_empty() {
        return Ok((CompactPlan::default(), String::new()));
    }
    let client = DirectHarness::new(DirectConfig {
        base_url: api_base.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        ..Default::default()
    })
    .map_err(|e| format!("client: {e}"))?;

    let tool: swarm_harness::tools::ToolDef =
        serde_json::from_value(compact_tool()).map_err(|e| format!("tool schema: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;

    // Up to three attempts. The endpoint this was built against does not always
    // obey the same instruction twice: one run would correctly drop progress
    // chatter, the next would copy the rendered `[FACT]` marker into the output.
    // Retrying is cheaper and more honest than loosening the validator until it
    // accepts bad output.
    let mut last_err = String::from("no attempt made");
    for attempt in 1..=3u32 {
        let mut req = ChatRequest::new(
            model,
            vec![
                LoopMessage {
                    role: "system".into(),
                    content: COMPACT_SYSTEM.into(),
                    ..LoopMessage::user("")
                },
                LoopMessage::user(render_notes(notes)),
            ],
        );
        req.tools = vec![tool.clone()];
        req.tool_choice = "required".into();
        req.max_tokens = 4096;

        let resp = match rt.block_on(async { client.chat(req).await }) {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("model: {e}");
                continue;
            }
        };
        let msg = resp.choices.first().map(|c| c.message.clone()).unwrap_or_default();
        let raw = msg
            .tool_calls
            .first()
            .map(|tc| tc.function.arguments.clone())
            .filter(|a| !a.trim().is_empty())
            .or_else(|| msg.content.clone())
            .unwrap_or_default();
        if raw.trim().is_empty() {
            last_err = "model returned neither a tool call nor content".into();
            continue;
        }
        match parse_plan(&raw).and_then(|p| verify_coverage(notes, &p).map(|_| p)) {
            Ok(plan) => return Ok((plan, raw)),
            Err(e) => {
                last_err = e;
                eprintln!("[compact] attempt {attempt}/3 rejected: {last_err}");
            }
        }
    }
    Err(format!("gave up after 3 attempts: {last_err}"))
}

/// Apply a verified plan: insert merges, delete drops and the merged originals.
///
/// Insert-then-delete order matters. A crash between the two leaves duplicated
/// notes (visible, fixable) rather than missing ones (silent, expensive).
pub fn apply_plan(
    store: &MemoryStore,
    scope_filter: Option<&str>,
    notes: &[Memory],
    plan: &CompactPlan,
) -> Result<(usize, usize), String> {
    let by_id: std::collections::HashMap<i64, &Memory> =
        notes.iter().map(|m| (m.id, m)).collect();

    let mut inserted = 0usize;
    for (text, ids) in &plan.merged {
        // The merged note inherits the scope of its first source: compaction runs
        // per scope, so all sources share one.
        let scope = ids
            .first()
            .and_then(|id| by_id.get(id))
            .map(|m| m.scope.clone())
            .or_else(|| scope_filter.map(|s| s.to_string()))
            .ok_or("merged note has no derivable scope")?;
        let kind = if ids
            .iter()
            .filter_map(|id| by_id.get(id))
            .any(|m| DURABLE_KINDS.contains(&m.kind.as_str()))
        {
            "FAIL"
        } else {
            "FACT"
        };
        store
            .remember(&scope, kind, text, &[], "compacted")
            .map_err(|e| e.to_string())?;
        inserted += 1;
    }

    let mut removed = 0usize;
    for id in plan.drop.iter().chain(plan.merged.iter().flat_map(|(_, ids)| ids)) {
        if store.forget(*id).map_err(|e| e.to_string())? {
            removed += 1;
        }
    }
    Ok((inserted, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, kind: &str, text: &str) -> Memory {
        Memory {
            id,
            scope: "global".into(),
            kind: kind.into(),
            text: text.into(),
            tags: vec![],
            source: "test".into(),
            created_at: 0,
            hits: 0,
        }
    }

    #[test]
    fn parses_plain_json() {
        let p = parse_plan(r#"{"merged":[{"text":"a","ids":[1,2]}],"drop":[3]}"#).unwrap();
        assert_eq!(p.merged.len(), 1);
        assert_eq!(p.merged[0].1, vec![1, 2]);
        assert_eq!(p.drop, vec![3]);
    }

    #[test]
    fn parses_json_inside_a_code_fence_with_chatter() {
        let raw = "Here you go:\n```json\n{\"merged\":[{\"text\":\"a\",\"ids\":[1]}],\"drop\":[]}\n```\nDone.";
        let p = parse_plan(raw).unwrap();
        assert_eq!(p.merged.len(), 1);
    }

    #[test]
    fn rejects_a_plan_that_forgets_a_note() {
        let notes = vec![note(1, "FACT", "a"), note(2, "FACT", "b")];
        let plan = CompactPlan {
            merged: vec![("only a".into(), vec![1])],
            drop: vec![],
            kept: vec![],
        };
        let err = verify_coverage(&notes, &plan).unwrap_err();
        assert!(err.contains("not accounted for"), "{err}");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let notes = vec![note(1, "FACT", "a")];
        let plan = CompactPlan {
            merged: vec![("x".into(), vec![1])],
            drop: vec![1],
            kept: vec![],
        };
        assert!(verify_coverage(&notes, &plan).is_err());
    }

    #[test]
    fn refuses_to_drop_a_fail_note() {
        let notes = vec![note(1, "FAIL", "endpoint 500s even with a valid signature")];
        let plan = CompactPlan {
            merged: vec![],
            drop: vec![1],
            kept: vec![],
        };
        let err = verify_coverage(&notes, &plan).unwrap_err();
        assert!(err.contains("must be merged"), "{err}");
    }

    #[test]
    fn accepts_a_plan_that_merges_a_fail_note() {
        let notes = vec![note(1, "FAIL", "endpoint 500s")];
        let plan = CompactPlan {
            merged: vec![("endpoint 500s even with a valid signature".into(), vec![1])],
            drop: vec![],
            kept: vec![],
        };
        assert!(verify_coverage(&notes, &plan).is_ok());
    }








    #[test]
    fn rejects_a_verbatim_echo_across_two_sources() {
        let notes = vec![
            note(1, "FACT", "扫描第 1 轮进行中，worker w1 正在处理分片"),
            note(2, "FACT", "扫描第 2 轮进行中，worker w2 正在处理分片"),
        ];
        // Copying the first source and claiming it covers both is not a merge.
        let plan = CompactPlan {
            merged: vec![(
                "扫描第 1 轮进行中，worker w1 正在处理分片".into(),
                vec![1, 2],
            )],
            drop: vec![],
            kept: vec![],
        };
        let err = verify_coverage(&notes, &plan).unwrap_err();
        assert!(err.contains("verbatim echo"), "{err}");
    }

    #[test]
    fn accepts_a_single_source_passthrough() {
        // One tight note with nothing to merge against: passing it through
        // unchanged is correct, not lazy. Forcing a rewrite would only add words.
        let notes = vec![note(1, "FACT", "Playwright 在 Windows 上需要 --no-sandbox 否则启动失败")];
        let plan = CompactPlan {
            merged: vec![(
                "Playwright 在 Windows 上需要 --no-sandbox 否则启动失败".into(),
                vec![1],
            )],
            drop: vec![],
            kept: vec![],
        };
        assert!(verify_coverage(&notes, &plan).is_ok());
    }
    #[test]
    fn accepts_a_real_rewrite() {
        let notes = vec![
            note(1, "FACT", "扫描第 1 轮进行中，worker w1 正在处理分片"),
            note(2, "FACT", "扫描第 2 轮进行中，worker w2 正在处理分片"),
        ];
        let plan = CompactPlan {
            merged: vec![("分片扫描按轮次推进，每轮由不同 worker 处理".into(), vec![1, 2])],
            drop: vec![],
            kept: vec![],
        };
        assert!(verify_coverage(&notes, &plan).is_ok());
    }
}

