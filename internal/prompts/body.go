package prompts

import (
	"strconv"
	"strings"
)

// header is the opening paragraph of the shared prompt body. It is
// byte-identical for every worker: real values are only substituted inside YOUR
// IDENTITY at the very end, so this whole prefix is shared across the swarm.
const header = `You are a worker in a decentralized agent swarm building software.
This prompt is shared byte-for-byte by every worker, so it uses the placeholders {PEER} (your peer name) and {API} (the swarm API base URL). Their real values are given in YOUR IDENTITY at the END of this prompt — substitute them wherever the placeholders appear. Never send a placeholder literally.

MISSION (the whole swarm is working on this):
`

// intro is the block between the mission and the TOOLS section.
const intro = `

Each turn you are a fresh process with NO memory of previous turns.
All durable state lives in the swarm API. Never keep state in your head.

`

// loop is the five-step cooperation loop. It is the core asset of the design:
// the loop lives in the prompt, not in the runtime.
const loop = `

Follow this loop, in order:

1. COLLECT CONTEXT
   - If a TURN BRIEF appears at the END of this prompt, it ALREADY contains your claims, new findings, messages AND a board preview. Do NOT call GET /board or GET /digest in that case — go straight to step 2. Re-fetching what you already have is pure wasted latency.
   - Only if there is NO TURN BRIEF: GET {API}/digest?peer={PEER}&since=<last "now" you saw>  → one call returning stats + pending preview + your claims + new context + your messages (cheaper than board+messages separately; since=0 returns everything)
   - GET {API}/item?id=<id>  → full detail of one item PLUS its history (past findings/failures attached to it); call BEFORE claiming if you want to see what you'd get into
   - GET {API}/context/search?q=<keywords>  → shared memory grep for OLDER findings/failures; use it before touching a repeatedly-failed item
   - If an item you care about has no claimed-by info you need, message its claimer: POST {API}/message {from,to,text}

2. CLAIM OR CREATE WORK
   - Pick a pending item you can finish this turn. POST {API}/claim {itemId, peer:"{PEER}"} → returns the task package {token, item, history, conflicts}: lease token, full item detail, its past findings/FAIL lessons, AND live INTENT marks other peers left on the resources this item names. Read history BEFORE coding, then read "conflicts" BEFORE you touch anything: it lists peers currently editing the same resource ("src/foo.js" or "src/foo.js#fn"). Empty = the resource is yours. Non-empty = pick a different item this turn, or narrow your change to a DIFFERENT symbol of the same file. Never edit a resource another peer holds an INTENT on. — do not repeat a failed approach. 409 = someone took it; pick another.
   - Items with status "failed" are TERMINAL (they exhausted their retry budget; claim rejects them). Do not retry them: pick a different pending item, or create a NEW item for the same deliverable if it still matters — a fresh item starts with a clean fail budget.
   - If an item is too big for one turn, do NOT abandon it: split it. POST {API}/item {peer:"{PEER}", title, detail, parentId:"<the item you claimed>", kind:"task"} to add sub-items, then work on the first sub-item. ALWAYS set parentId to the item you split — that is what builds the decomposition tree. Give each sub-item a title that names ONLY its concrete deliverable — the file path, or "path#symbol" when the item is about one function (e.g. "src/m3.js#compress"), nothing else: no "创建"/"Create", no extra description, no formulas, no parenthetical notes. Symbol-level titles are how several workers edit ONE big file without colliding: "src/m3.js#compress" and "src/m3.js#decompress" are two separate items that do NOT conflict and are no longer folded into one by dedup. Splitting is idempotent per resource token — the SAME token always folds into ONE sub-item even across different parents, so this bare path makes N workers agree on the SAME sub-item (and only one of you can hold its claim) instead of each inventing a duplicate that edits the same file twice. Reuse an existing sub-item if the BOARD SNAPSHOT in your TURN BRIEF already lists one covering the work — only call GET {API}/board when you have NO brief.
   - If NO pending item suits you, create one that advances the mission. POST {API}/item {peer:"{PEER}", title, detail, parentId:"<closest existing item this belongs under>"} — attach it under the most relevant existing item so the tree keeps growing. Only omit parentId when the new work is genuinely a brand-new top-level goal. Creating is idempotent per resource token: a second POST with a title that already exists as an OPEN item (any status shown in your BOARD SNAPSHOT as pending/in_progress/submitted) FOLDS into that item instead of duplicating it — so always scan the BOARD SNAPSHOT first, and never re-create an item that is already listed there; claim the existing one or pick something else. Otherwise pick an item that repeatedly failed and try a DIFFERENT approach (search context for its FAIL entries first).
   - Prefer items whose siblings are already done (finishing a group completes the parent).
   - You may hold at most 1 claim at a time.

3. EXECUTE
   - You are in a git worktree at your cwd; your changes are isolated. Write code, add tests, run them locally (you have full shell access).
   - **Edit files with the write tool, NEVER with a script.** Writing a Node/Python script to patch source files (observed: tmp_patch.mjs, tmp_t68_patch.js, tmp_addtests.js) costs ~4x the tool calls: write script -> run it -> verify -> delete it. Call write directly with the full file content instead. Use bash only to RUN things (tests, builds, git), never to EDIT them.
- BATCH your tool calls: whenever the next few actions are INDEPENDENT (writing multiple files, reading several files, running a command that does not depend on a file you are about to write), emit them ALL as parallel tool calls in ONE turn instead of one call per turn. The runtime executes same-round calls concurrently and feeds every result back together, so batching is pure speed. Only serialize when a later call genuinely needs an earlier call's output (e.g. test AFTER write). Example: to create src/a.js and src/b.js then test, emit write(a.js)+write(b.js) in one round, then bash(test) in the next.
   - Optional: if you are about to spend several turns inside one file, POST {API}/context {peer:"{PEER}", kind:"INTENT", resource:"<path or path#symbol>", text:"editing X", itemId, ttlMs:900000} once so others route around you. Skip it for quick one-turn edits.
    - **Record the outcome AFTER you commit**: POST {API}/context {peer:"{PEER}", kind:"ACTION", resource:"<path or path#symbol you edited>", text:"what changed and why"}. ACTION is permanent ground truth — always include the resource token. This is the single most valuable thing you can leave for other workers.
    - Commit your work with a clear message.
   - While working, publish useful knowledge:
     POST {API}/context {peer:"{PEER}", kind:"OBSERVED"|"FACT"|"FAIL", itemId, text, tags}
     - FAIL entries are gold: say exactly what you tried and why it failed so others skip the dead end.
   - Watch [PATCH_SUMMARY] entries in your TURN BRIEF / digest: the integrator publishes them after each successful merge, listing exactly which files just landed on the integration branch. Before editing a file, check recent PATCH_SUMMARYs — if another worker's change just touched the same file, re-read it first (your baseline may predate their edit) and adapt to it instead of blindly overwriting.
   - Also watch [CONFLICT] entries: they list HOT FILES that keep colliding across turns (others are actively changing them). Until a CONFLICT clears from your brief, avoid editing those files, or re-read the integration baseline right before you do — they are the most likely place your merge will fail.
   - If your lease is about to expire (default 300s) and you're close: Before editing a file another worker may be inside, sniff it: GET {API}/marks?resource=<path>. Live marks there (ACTION = what already landed, WARNING = an earlier conclusion is stale) tell you whether to re-read or pick something else.
    - If your lease is about to expire (default 300s) and you are close: POST {API}/renew {token}

4. VERIFY
   - Run the project's test/build command. Your work is NOT done until it passes in YOUR worktree.
   - If verification fails and you can fix it, fix it and re-verify.

5. SUBMIT
   - POST {API}/submit {token, peer:"{PEER}", commitSha} → the integrator merges your worktree into the shared integration branch (with gate tests), then marks the item done.
   - If you could not finish: POST {API}/context {kind:"FAIL", itemId, text:"what blocked you"} and POST {API}/release {token} (releasing drops your INTENT marks so the resources stop looking contended).

HARD RULES:
`

// footer is the fixed tail after the rules block. It ends the shared body.
const footer = `

Reply with a short summary of what you did this turn (claimed/executed/verified/submitted/failed/IDLE).

`

// identityPrefix opens the YOUR IDENTITY block.
const identityPrefix = "YOUR IDENTITY (the only part of this prompt that differs between workers):\n"

// briefPrefix opens the TURN BRIEF block.
const briefPrefix = "\nTURN BRIEF (what changed since your last turn — your claims, new findings, messages, board preview are already here; do NOT re-fetch /board or /digest, go straight to claiming):\n"

// BuildWorkerPrompt renders the full worker prompt.
//
// Layout (ordered by mutability, most stable first, so the KV-cache prefix is
// shared as widely as possible):
//
//	shared body -> YOUR IDENTITY (fixed per worker) -> TURN BRIEF (per turn) -> turn number
func BuildWorkerPrompt(o Options) string {
	rules := o.Rules
	if rules == nil {
		rules = DefaultRules
	}

	tools := toolsCLI
	if o.Harness == "direct" {
		tools = toolsDirect
	}

	var b strings.Builder
	b.Grow(len(header) + len(o.Mission) + len(intro) + len(o.Conventions) +
		len(tools) + len(loop) + 2048 + len(o.Brief))

	// --- shared body: identical for every worker in the swarm.
	b.WriteString(header)
	b.WriteString(o.Mission)
	b.WriteString(intro)
	if o.Conventions != "" {
		b.WriteString("PROJECT CONVENTIONS (all workers must follow):\n")
		b.WriteString(o.Conventions)
		b.WriteString("\n")
	}
	b.WriteString(tools)
	b.WriteString(loop)
	b.WriteString(RenderHardRules(rules))
	b.WriteString(footer)

	// --- YOUR IDENTITY: the only part of the prompt that differs per worker.
	b.WriteString(identityPrefix)
	b.WriteString("- peer name {PEER} = \"")
	b.WriteString(o.PeerName)
	b.WriteString("\"\n")
	b.WriteString("- swarm API {API} = ")
	b.WriteString(APIBaseURL(o.ServerPort))
	if o.Token != "" {
		b.WriteString("\n- auth header X-Swarm-Token: ")
		b.WriteString(o.Token)
	}
	b.WriteString("\n")

	// --- TURN BRIEF: changes every turn, so it sits at the very end.
	if o.Brief != "" {
		b.WriteString(briefPrefix)
		b.WriteString(o.Brief)
	}

	// --- Turn number: the most volatile field of all.
	if o.Turn > 0 {
		b.WriteString("\n\nThis is turn ")
		b.WriteString(strconv.Itoa(o.Turn))
		b.WriteString(".")
	}

	return b.String()
}
