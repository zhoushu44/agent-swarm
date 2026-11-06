//! Worker prompt construction, byte-compatible with Go `internal/prompts`.
//!
//! The shared body (protocol, rules, tools, loop) is captured verbatim from the
//! Go renderer into `body_gen.rs`, because a single stray space changes the
//! prompt and therefore model behaviour. `tests/golden.rs` compares the whole
//! rendered prompt against fixtures produced by the Go binary, so any drift
//! fails the build instead of silently changing how workers behave.
//!
//! Ordering is deliberate and must not be "tidied":
//!
//! ```text
//! shared body -> YOUR IDENTITY (fixed per worker) -> TURN BRIEF (per turn) -> turn number
//! ```
//!
//! That ordering is what makes the prefix KV-cache friendly: the volatile tail
//! grows while the shared prefix stays byte-identical across the swarm.

mod body_gen;

pub use body_gen::{BODY_CLI, BODY_DIRECT};

/// One HARD RULE plus the metadata needed for conflict detection.
///
/// `priority`: 0 = highest (safety red line), 5 = medium, 10 = low (advisory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub priority: u8,
    pub category: &'static str,
    pub text: &'static str,
    pub rationale: &'static str,
}

/// The rule registry, copied from Go `prompts.go` L37-... verbatim.
///
/// Order matters: `render_hard_rules` sorts by (priority, id), and the golden
/// fixtures were produced with this exact set.
pub const DEFAULT_RULES: &[Rule] = &[
    Rule {
        id: "one-claim-at-a-time",
        priority: 0,
        category: "claiming",
        text: "One claim at a time. Never touch files outside your worktree.",
        rationale: "Concurrency safety: multiple workers editing the same file causes integration conflicts.",
    },
    Rule {
        id: "no-self-done",
        priority: 0,
        category: "submission",
        text: "Never mark your own work done - submission + gate decides that.",
        rationale: "Prevents a worker from bypassing the gate and self-declaring completion.",
    },
    Rule {
        id: "accept-409",
        priority: 1,
        category: "claiming",
        text: "If claim/submit returns 409, accept it and move on; races are normal, not errors.",
        rationale: "409 on claim/submit is normal contention between workers, not a system error.",
    },
    Rule {
        id: "small-verifiable-turns",
        priority: 5,
        category: "execution",
        text: "Keep each turn focused: finish something small and verifiable rather than half-finishing something big.",
        rationale: "Small steps lower integration conflict probability and give faster gate feedback.",
    },
    Rule {
        id: "no-scratch-files",
        priority: 0,
        category: "submission",
        text: "NEVER commit scratch/debug files: no probe.*, tmp_*.*, debug_*.*, scratch/, *.log, *.tmp, *.bak. Delete them BEFORE git commit; the gate rejects merges containing them, and every rejected merge wastes a full turn. Only commit real deliverables (source, tests, docs, config).",
        rationale: "Scratch files pollute history and are rejected by the gate, wasting turns.",
    },
    Rule {
        id: "submit-is-the-only-door",
        priority: 0,
        category: "submission",
        text: "POST /submit is the ONLY way your work reaches the shared branch. Nothing else you do is visible to the swarm: no direct git push, no editing another worktree, no writing outside your own worktree. If the work is not submitted, it does not exist.",
        rationale: "A blacklist of forbidden writes can never be complete; a whitelist of the one allowed exit can. This is agent-fleet-manager's 'reconcile is the only door into canonical state' applied to the swarm's API, and it is what makes a crashed or misbehaving worker harmless — it can only ever have written inside its own worktree.",
    },
    Rule {
        id: "idle-when-no-work",
        priority: 5,
        category: "execution",
        text: "When the board has no suitable work left for you, reply exactly: IDLE",
        rationale: "Spinning idle burns API quota; an explicit IDLE lets the runner back off.",
    },
    Rule {
        id: "always-call-a-tool",
        priority: 0,
        category: "execution",
        text: "NEVER reply with plain text alone. Every turn MUST make at least one tool call (bash/write/http). Plain text performs no work and submits nothing; if you believe the task is done, call http to POST /submit, or finish(summary) only after submitting.",
        rationale: "Some endpoints ignore tool_choice:\"required\" and answer in prose; without this the worker exits having done nothing.",
    },
];

/// The `--no-create` rule, appended when item creation is forbidden.
///
/// Go appends this at `runner.go` L666-674, after `DefaultRules`.
pub const NO_CREATE_RULE: Rule = Rule {
    id: "no-create-items",
    priority: 0,
    category: "claiming",
    text: "Do NOT create new items (no POST /item). Work only on what the board already lists. If nothing suits you, finish the turn with IDLE.",
    rationale: "Keeps the board bounded so --exit-when-done can fire; workers otherwise invent new top-level work indefinitely.",
};

/// Options for `build_worker_prompt`, mirroring Go `prompts.Options`.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The worker's peer name, substituted into YOUR IDENTITY.
    pub peer_name: String,
    /// The swarm API port; the base URL is `http://127.0.0.1:<port>`.
    pub server_port: u16,
    /// Optional; when set it is exposed as the `X-Swarm-Token` auth header.
    pub token: String,
    /// The whole-swarm objective.
    pub mission: String,
    /// Optional project-wide conventions.
    pub conventions: String,
    /// 1-based turn number (0 omits the trailing turn line).
    pub turn: u32,
    /// The per-turn TURN BRIEF block (empty on the first turn).
    pub brief: String,
    /// `direct` gets the dedicated http tool; anything else gets the curl block.
    pub harness: String,
    /// Overrides `DEFAULT_RULES` when non-empty.
    pub rules: Vec<Rule>,
    /// Definition of Done for the item being worked on, already rendered as a
    /// prompt block. Empty omits it.
    ///
    /// Deliberately *not* a structured field on `Item`: the schema is shared with
    /// Go and must stay byte-compatible (README constraint 1). Instead the runner
    /// parses a `DoD:` paragraph out of `item.detail` and hands it over here, so
    /// the worker sees what "done" means before it starts instead of guessing.
    pub dod: String,
    /// Cross-run notes, already rendered as bullet lines (no header).
    ///
    /// Empty omits the whole block, which is what keeps the golden fixtures
    /// byte-identical when memory is disabled. When present it is spliced in
    /// AFTER the conventions and BEFORE the tools block — it is the same kind of
    /// thing: standing instructions that outrank improvisation but rank below
    /// the task itself.
    pub memory: String,
}

/// The Definition-of-Done block prefix.
const DOD_PREFIX: &str = "DEFINITION OF DONE (the operator's acceptance criteria — do not submit until each line is true):\n";

/// Extract a `DoD:` paragraph from an item's detail text.
///
/// The convention, not a schema change: an item's `detail` may contain
///
/// ```text
/// DoD:
/// - cargo test --workspace passes
/// - no new warnings
/// ```
///
/// Everything from the marker to the end of the block is the acceptance criteria.
/// A marker with nothing after it yields an empty string, so a stray `DoD:` in
/// prose costs nothing. Returns the rendered block (with prefix) or `""`.
pub fn extract_dod(detail: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in detail.lines() {
        let trimmed = line.trim();
        if !in_block {
            // Accept "DoD:" and "Definition of Done:" as the marker.
            if trimmed == "DoD:" || trimmed.eq_ignore_ascii_case("Definition of Done:") {
                in_block = true;
            }
            continue;
        }
        // The block ends at the first blank line or a line that is neither a
        // bullet nor an indented continuation.
        if trimmed.is_empty() {
            break;
        }
        if !(trimmed.starts_with('-') || trimmed.starts_with('*') || line.starts_with(' ')) {
            break;
        }
        lines.push(trimmed);
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(DOD_PREFIX.len() + 64 * lines.len());
    out.push_str(DOD_PREFIX);
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The swarm API base URL for a server port (Go `APIBaseURL`).
pub fn api_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// Render the HARD RULES block (Go `RenderHardRules`, prompts.go L185-201).
///
/// Note there is NO banner line: the caller's body already contains
/// `HARD RULES:\n`, and this function only emits the `- <text>` lines joined by
/// newlines. Stable sort by (priority, id).
pub fn render_hard_rules(rules: &[Rule]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&Rule> = rules.iter().collect();
    // `sort_by` is stable, matching Go's `sort.SliceStable`.
    sorted.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(b.id)));
    sorted
        .iter()
        .map(|r| format!("- {}", r.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The identity block prefix (Go `identityPrefix`, body.go L86).
const IDENTITY_PREFIX: &str =
    "YOUR IDENTITY (the only part of this prompt that differs between workers):\n";

/// The brief block prefix (Go `briefPrefix`, body.go L89).
const BRIEF_PREFIX: &str = "\nTURN BRIEF (what changed since your last turn — your claims, new findings, messages, board preview are already here; do NOT re-fetch /board or /digest, go straight to claiming):\n";

/// Build the full worker prompt.
///
/// Byte-compatible with Go `BuildWorkerPrompt`. The shared body comes from the
/// generated constant; everything after it is assembled here in the same order
/// as the Go builder.
pub fn build_worker_prompt(o: &Options) -> String {
    let rules: Vec<Rule> = if o.rules.is_empty() {
        DEFAULT_RULES.to_vec()
    } else {
        o.rules.clone()
    };

    let body = if o.harness == "direct" {
        BODY_DIRECT
    } else {
        BODY_CLI
    };

    // The generated body was captured with an EMPTY mission, so re-insert the
    // mission right after the header line. The header ends with
    // "MISSION (the whole swarm is working on this):\n" — the mission text
    // follows it immediately.
    let mut out = String::with_capacity(body.len() + o.mission.len() + o.conventions.len() + 2048);

    const MISSION_MARKER: &str = "MISSION (the whole swarm is working on this):\n";
    match body.find(MISSION_MARKER) {
        Some(i) => {
            let cut = i + MISSION_MARKER.len();
            out.push_str(&body[..cut]);
            out.push_str(&o.mission);
            out.push_str(&body[cut..]);
        }
        None => out.push_str(body),
    }

    // Conventions sit right after the intro block and before the TOOLS block.
    // Go writes them with NO surrounding blank line (body.go L116-120):
    //   "PROJECT CONVENTIONS (all workers must follow):\n" + conventions + "\n"
    // and the TOOLS block follows immediately.
    if !o.conventions.is_empty() {
        const TOOLS_MARKER: &str = "TOOLS:";
        match out.find(TOOLS_MARKER) {
            Some(i) => {
                let mut spliced = String::with_capacity(out.len() + o.conventions.len() + 64);
                spliced.push_str(&out[..i]);
                spliced.push_str("PROJECT CONVENTIONS (all workers must follow):\n");
                spliced.push_str(&o.conventions);
                spliced.push('\n');
                spliced.push_str(&out[i..]);
                out = spliced;
            }
            None => {
                out.push_str("PROJECT CONVENTIONS (all workers must follow):\n");
                out.push_str(&o.conventions);
                out.push('\n');
            }
        }
    }

    // Cross-run memory sits after the conventions and before the tools block.
    //
    // Deliberately conditional: with no notes the prompt is byte-identical to
    // what the golden fixtures captured, so memory being off cannot change model
    // behaviour by accident.
    if !o.memory.trim().is_empty() {
        const TOOLS_MARKER: &str = "TOOLS:";
        let block = format!(
            "CROSS-RUN MEMORY (notes from earlier sessions — treat as strong hints, not gospel;\n\
             the code in front of you always wins if they disagree):\n{}\n",
            o.memory.trim_end()
        );
        match out.find(TOOLS_MARKER) {
            Some(i) => {
                let mut spliced = String::with_capacity(out.len() + block.len());
                spliced.push_str(&out[..i]);
                spliced.push_str(&block);
                spliced.push_str(&out[i..]);
                out = spliced;
            }
            None => out.push_str(&block),
        }
    }

    // The captured body carries a rendered HARD RULES block. When the caller
    // supplies a different rule set (`--no-create` appends an 8th), the block
    // must be rebuilt from the caller's rules.
    //
    // It used to locate the block by searching for `render_hard_rules(DEFAULT_RULES)`
    // as a literal substring. That NEVER matched: `body_gen.rs` was captured from
    // the Go renderer, which emits the rules in its own order, while
    // `render_hard_rules` sorts by `(priority, id)`. So the search silently
    // failed and `--no-create` never reached a single worker — a bug that sat
    // behind a green test because the test only asserted the rule's text was
    // absent/present in a prompt built WITHOUT the replacement path.
    //
    // Now the block is delimited by its banner and the following blank line, so
    // the replacement does not depend on the rules being in any particular
    // order. If the delimiters are missing the build fails loudly instead of
    // silently dropping a rule.
    let rules_are_default = rules.len() == DEFAULT_RULES.len()
        && rules.iter().zip(DEFAULT_RULES).all(|(a, b)| a.id == b.id);
    if !rules_are_default {
        const RULES_BANNER: &str = "HARD RULES:\n";
        if let Some(start) = out.find(RULES_BANNER) {
            let body_start = start + RULES_BANNER.len();
            // The block runs to the first blank line, which is how the captured
            // body terminates it.
            let body_end = out[body_start..]
                .find("\n\n")
                .map(|i| body_start + i + 1)
                .unwrap_or(out.len());
            let mut rebuilt = String::with_capacity(out.len() + 256);
            rebuilt.push_str(&out[..body_start]);
            rebuilt.push_str(&render_hard_rules(&rules));
            rebuilt.push_str(&out[body_end..]);
            out = rebuilt;
        } else {
            // No banner means the body changed shape. Refuse to guess: a silently
            // dropped rule is worse than a loud failure.
            debug_assert!(
                false,
                "HARD RULES banner missing from the captured body; \
                 the rule block cannot be rebuilt"
            );
        }
    }

    // --- YOUR IDENTITY: the only part that differs per worker.
    // The shared body ends without a trailing blank line, and Go's
    // `identityPrefix` has no leading newline, so the separator is supplied
    // here (body.go L127).
    out.push_str("\n\n");
    out.push_str(IDENTITY_PREFIX);
    out.push_str("- peer name {PEER} = \"");
    out.push_str(&o.peer_name);
    out.push_str("\"\n- swarm API {API} = ");
    out.push_str(&api_base_url(o.server_port));
    if !o.token.is_empty() {
        out.push_str("\n- auth header X-Swarm-Token: ");
        out.push_str(&o.token);
    }
    out.push('\n');

    // --- DEFINITION OF DONE, when the item carries one.
    // Placed before the brief: the criteria are stable for the life of the item,
    // while the brief changes every turn. Keeping the stable text ahead of the
    // volatile text preserves the KV-cache-friendly ordering documented above.
    if !o.dod.is_empty() {
        out.push('\n');
        out.push_str(&o.dod);
    }

    // --- TURN BRIEF: changes every turn, so it sits at the very end.
    if !o.brief.is_empty() {
        out.push_str(BRIEF_PREFIX);
        out.push_str(&o.brief);
    }

    // --- Turn number: the most volatile field of all.
    if o.turn > 0 {
        out.push_str("\n\nThis is turn ");
        out.push_str(&o.turn.to_string());
        out.push('.');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_ordered_by_priority_then_id() {
        let s = render_hard_rules(DEFAULT_RULES);
        // priority 0 rules must come before priority 5 ones
        let p0 = s.find("One claim at a time").unwrap();
        let p1 = s.find("409").unwrap();
        let p5 = s.find("Keep each turn focused").unwrap();
        assert!(p0 < p1, "priority 0 before priority 1");
        assert!(p1 < p5, "priority 1 before priority 5");
        // no banner line: the block is only "- <text>" lines
        assert!(s.starts_with("- "), "no banner, starts with a bullet");
    }

    #[test]
    fn empty_rules_render_empty() {
        assert_eq!(render_hard_rules(&[]), "");
    }

    #[test]
    fn api_base_url_shape() {
        assert_eq!(api_base_url(17771), "http://127.0.0.1:17771");
    }

    #[test]
    fn dod_extracts_a_bulleted_block() {
        let d = extract_dod("Fix the thing.\n\nDoD:\n- cargo test passes\n- no new warnings\n");
        assert!(d.starts_with("DEFINITION OF DONE"), "got: {d}");
        assert!(d.contains("- cargo test passes"));
        assert!(d.contains("- no new warnings"));
    }

    #[test]
    fn dod_marker_without_content_is_empty() {
        assert_eq!(extract_dod("DoD:\n"), "");
        assert_eq!(extract_dod("no marker at all"), "");
        // A marker mentioned in prose, followed by more prose, is not a block.
        assert_eq!(extract_dod("see the DoD: section for details"), "");
    }

    #[test]
    fn dod_stops_at_a_blank_line() {
        let d = extract_dod("DoD:\n- one\n- two\n\nAfterthought that is not a criterion.");
        assert!(d.contains("- one"));
        assert!(d.contains("- two"));
        assert!(!d.contains("Afterthought"), "the block must end at the blank line");
    }

    #[test]
    fn dod_accepts_the_long_form_marker() {
        let d = extract_dod("Definition of Done:\n- builds");
        assert!(d.contains("- builds"), "got: {d}");
    }

    #[test]
    fn dod_is_spliced_before_the_brief() {
        // The criteria are stable for the item's life; the brief changes every
        // turn. Stable text must precede volatile text to stay KV-cache friendly.
        let with_dod = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1234,
            harness: "direct".into(),
            dod: extract_dod("DoD:\n- builds\n"),
            brief: "BOARD SNAPSHOT: 1 pending".into(),
            ..Default::default()
        });
        let dod_at = with_dod.find("DEFINITION OF DONE").unwrap();
        // Match the block prefix, not the phrase: the shared body discusses the
        // TURN BRIEF in prose long before the actual block is appended.
        let brief_at = with_dod.find("TURN BRIEF (what changed").unwrap();
        assert!(dod_at < brief_at, "stable DoD must precede the volatile brief");
    }

    #[test]
    fn empty_dod_adds_nothing() {
        // This is what keeps the golden fixtures byte-identical: the default
        // options carry no DoD, so the prompt is unchanged.
        let base = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1234,
            harness: "direct".into(),
            ..Default::default()
        });
        assert!(!base.contains("DEFINITION OF DONE"));
    }

    #[test]
    fn identity_carries_peer_and_api() {
        let p = build_worker_prompt(&Options {
            peer_name: "w7".into(),
            server_port: 1234,
            harness: "direct".into(),
            ..Default::default()
        });
        assert!(p.contains("- peer name {PEER} = \"w7\""));
        assert!(p.contains("- swarm API {API} = http://127.0.0.1:1234"));
        assert!(!p.contains("This is turn"), "turn 0 must omit the line");
    }

    #[test]
    fn token_adds_auth_header_line() {
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            token: "tok".into(),
            harness: "direct".into(),
            ..Default::default()
        });
        assert!(p.contains("- auth header X-Swarm-Token: tok"));
    }

    #[test]
    fn turn_line_is_appended_last() {
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            turn: 4,
            harness: "direct".into(),
            ..Default::default()
        });
        assert!(p.ends_with("\n\nThis is turn 4."));
    }

    #[test]
    fn brief_sits_before_turn_line() {
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            turn: 2,
            brief: "BRIEFBODY".into(),
            harness: "direct".into(),
            ..Default::default()
        });
        let brief_at = p.find("BRIEFBODY").unwrap();
        let turn_at = p.find("This is turn 2.").unwrap();
        assert!(brief_at < turn_at, "brief before turn line");
    }

    #[test]
    fn no_create_rule_appends_after_defaults() {
        let mut rules = DEFAULT_RULES.to_vec();
        rules.push(NO_CREATE_RULE);
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            harness: "direct".into(),
            rules,
            ..Default::default()
        });
        assert!(p.contains("Do NOT create new items"));
    }

    /// The `--no-create` rule must SURVIVE the rule-block rebuild.
    ///
    /// The rebuild used to locate the HARD RULES block by searching for
    /// `render_hard_rules(DEFAULT_RULES)` as a literal. That never matched —
    /// `body_gen.rs` was captured from the Go renderer, whose rule order differs
    /// from `render_hard_rules`' `(priority, id)` sort — so the search silently
    /// failed and the appended rule was dropped. The prompt a worker received
    /// under `--no-create` was byte-identical to one without it.
    ///
    /// This asserts both halves: the new rule is present, AND the default rules
    /// are still there (a rebuild that wipes the block would pass a weaker test).
    #[test]
    fn a_custom_rule_set_replaces_the_block_without_dropping_it() {
        let mut rules = DEFAULT_RULES.to_vec();
        rules.push(NO_CREATE_RULE);
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            harness: "direct".into(),
            rules,
            ..Default::default()
        });
        // The appended rule made it in.
        assert!(
            p.contains("Do NOT create new items"),
            "the --no-create rule must reach the prompt"
        );
        // ...and the defaults were not wiped by the rebuild.
        assert!(
            p.contains("One claim at a time"),
            "the rebuild must keep the default rules"
        );
        assert!(
            p.contains("POST /submit is the ONLY way"),
            "the whitelist rule must be present in the default set"
        );
        // Exactly one HARD RULES block: a botched splice would duplicate it.
        assert_eq!(
            p.matches("HARD RULES:").count(),
            1,
            "there must be exactly one rules block"
        );
    }

    /// The default rule set must not be rebuilt at all — the captured body is
    /// byte-identical to the Go output, and rewriting it would break the golden
    /// fixtures.
    #[test]
    fn default_rules_leave_the_captured_body_alone() {
        let p = build_worker_prompt(&Options {
            peer_name: "w1".into(),
            server_port: 1,
            harness: "direct".into(),
            ..Default::default()
        });
        // The captured body's own order (Go's), not render_hard_rules' sort.
        assert!(
            p.contains("HARD RULES:\n- NEVER reply with plain text alone"),
            "the default path must use the captured body verbatim"
        );
    }
}
