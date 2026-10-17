// worker prompt 协议 —— Agensh 的核心资产："五步合作循环"全部写在 prompt 里，
// 而不是硬编码在运行时。我们照此办理，运行时只负责把 API 地址和身份注入。
//
// prompt 里不塞任何全局状态（board 内容、别的 agent 干了什么）——
// agent 每回合自己拉。这样 prompt 大小与 agent 总数无关，规模可扩展。
//
// KV-cache/前缀缓存友好（省 token）：正文段（协议/规则/工具）里所有会随 worker
// 变化的东西（peerName、API 端口、token）一律用 {PEER}/{API} 占位符，使这段正文
// 对**所有 worker 逐字节相同**——对齐 Agensh 原论文 2.3 节的设计（"worker prompt
// is exactly the same for each worker except for the worker ID"）。
// 真正随 worker/turn 变化的内容按易变度从低到高排在尾部：
//   共享正文 → YOUR IDENTITY(每 worker 固定) → TURN BRIEF(每回合变) → turn 号
// 于是：跨 worker 共享"共享正文"这一整段前缀；同一 worker 跨回合共享到 IDENTITY 为止。

// ---------- HARD RULES 注册表 ----------
// 每条规则带 ID + priority + category + 冲突检测元数据。
// 加新规则时：① 在 RULES 里注册；② validateRules() 会在启动时检测冲突。
// priority: 0=最高（安全红线）, 5=中, 10=低（建议性）。
// conflicts: 规则 ID 数组，若两条规则同时启用且互相矛盾，validateRules 会抛错。
export const RULES = [
  {
    id: 'one-claim-at-a-time',
    priority: 0,
    category: 'claiming',
    text: 'One claim at a time. Never touch files outside your worktree.',
    rationale: '并发安全基石：多 worker 同时改同一文件会导致集成冲突。',
  },
  {
    id: 'no-self-done',
    priority: 0,
    category: 'submission',
    text: 'Never mark your own work "done" — submission + gate decides that.',
    rationale: '防止 worker 绕过门禁自判完成。',
  },
  {
    id: 'accept-409',
    priority: 1,
    category: 'claiming',
    text: 'If claim/submit returns 409, accept it and move on; races are normal, not errors.',
    rationale: '多 worker 并发认领/提交时 409 是正常竞争，不是系统错误。',
  },
  {
    id: 'small-verifiable-turns',
    priority: 5,
    category: 'execution',
    text: 'Keep each turn focused: finish something small and verifiable rather than half-finishing something big.',
    rationale: '小步快跑降低集成冲突概率，门禁反馈更快。',
  },
  {
    id: 'no-scratch-files',
    priority: 0,
    category: 'submission',
    text: 'NEVER commit scratch/debug files: no probe.*, tmp_*.*, debug_*.*, scratch/, *.log, *.tmp, *.bak. Delete them BEFORE git commit; the gate rejects merges containing them, and every rejected merge wastes a full turn. Only commit real deliverables (source, tests, docs, config).',
    rationale: 'scratch 文件污染仓库历史，门禁会拒 merge，浪费回合。',
  },
  {
    id: 'idle-when-no-work',
    priority: 5,
    category: 'execution',
    text: 'When the board has no suitable work left for you, reply exactly: IDLE',
    rationale: '空转烧 API 配额，明确 IDLE 让 runner 进入退避。',
  },
];

// 规则冲突检测：静态检查 RULES 里是否有互相矛盾的规则。
// 目前检测两类冲突：
//   ① 同一 category 里 priority 相同的规则数量过多（>3 条 worker 会困惑优先级）
//   ② 规则文本里出现互斥关键词（"always X" vs "never X"、"must first" vs "must last"）
// 返回 { ok: bool, warnings: string[], errors: string[] }
export function validateRules(rules = RULES) {
  const warnings = [];
  const errors = [];
  // ① 同 category 同 priority 过多
  const byCatPri = new Map();
  for (const r of rules) {
    const key = `${r.category}:${r.priority}`;
    if (!byCatPri.has(key)) byCatPri.set(key, []);
    byCatPri.get(key).push(r.id);
  }
  for (const [key, ids] of byCatPri) {
    if (ids.length > 3) warnings.push(`category:priority ${key} has ${ids.length} rules (>${3}): ${ids.join(', ')} — worker may not know which to follow first`);
  }
  // ② 互斥关键词
  const ALWAYS = /\b(always|must|required)\b/i;
  const NEVER = /\b(never|forbidden|must not)\b/i;
  for (let i = 0; i < rules.length; i++) {
    for (let j = i + 1; j < rules.length; j++) {
      const a = rules[i], b = rules[j];
      if (a.category !== b.category) continue;
      const aHasAlways = ALWAYS.test(a.text), aHasNever = NEVER.test(a.text);
      const bHasAlways = ALWAYS.test(b.text), bHasNever = NEVER.test(b.text);
      if ((aHasAlways && bHasNever) || (aHasNever && bHasAlways)) {
        // 粗粒度检测：如果两条规则都涉及同一个名词（如 "commit"），可能是冲突
        const aWords = new Set(a.text.toLowerCase().match(/\b\w{4,}\b/g) || []);
        const bWords = new Set(b.text.toLowerCase().match(/\b\w{4,}\b/g) || []);
        const common = [...aWords].filter((w) => bWords.has(w));
        if (common.length > 0) {
          warnings.push(`potential conflict between ${a.id} and ${b.id} (category=${a.category}): one says ALWAYS/MUST, other says NEVER/MUST NOT, both mention ${common.slice(0, 3).join('/')}`);
        }
      }
    }
  }
  return { ok: errors.length === 0, warnings, errors };
}

// 按 priority 排序、category 分组渲染 HARD RULES 段。
// 输出格式稳定（KV-cache 前缀命中依赖它），不按 worker/turn 变化。
export function renderHardRules(rules = RULES) {
  const sorted = [...rules].sort((a, b) => a.priority - b.priority || a.id.localeCompare(b.id));
  return sorted.map((r) => `- ${r.text}`).join('\n');
}

export function buildWorkerPrompt({ peerName, serverPort, token, mission, conventions, turn, brief = '', harness = 'direct' }) {
  const api = `http://127.0.0.1:${serverPort}`;
  // TOOLS 段按 harness 定制：direct 有专用 http 工具；pi/claude/codex 这类 CLI
  // 只有文件+shell 工具，API 调用必须经 curl（pi 的 bash 是 Git Bash，curl 可用）。
  // 两段文本都保持逐字节不变（KV-cache 前缀命中依赖它），故不内嵌 peerName/api。
  const toolsBlock = harness === 'direct'
    ? `TOOLS: you have three tools.
- "http" — the ONLY way to call the swarm API. Pass ONE string field "request" holding the full JSON request, e.g.
    http({request:"{\\"method\\":\\"GET\\",\\"path\\":\\"/board\\"}"})
    http({request:"{\\"method\\":\\"GET\\",\\"path\\":\\"/board\\",\\"status\\":\\"pending\\"}"})
    http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/claim\\",\\"itemId\\":\\"<id>\\",\\"peer\\":\\"{PEER}\\"}"})
    → returns {token, item, history}: the full task package — item detail AND its past findings/failures. Do NOT re-fetch /item or /context/search for it.
    http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/submit\\",\\"token\\":\\"<token>\\",\\"peer\\":\\"{PEER}\\",\\"commitSha\\":\\"<sha>\\"}"})
    http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/context\\",\\"peer\\":\\"{PEER}\\",\\"kind\\":\\"FACT\\",\\"text\\":\\"...\\" ,\\"itemId\\":\\"<id>\\"}"})
  Every POST parameter (itemId, peer, token, commitSha, ...) MUST be inside that JSON string. NEVER use curl/wget for the API (they hang on this host).
- "write" — create or overwrite a file in your worktree. ALWAYS use this for writing files; NEVER write files through the shell (cmd.exe truncates multi-line content and mangles quotes/parens — it will cost you several extra steps to fix).
    write({file:"{\\"path\\":\\"src/m3.js\\",\\"content\\":\\"function f(x) {\\\\n  return x;\\\\n}\\\\nmodule.exports = { f };\\\\n\\"}"})
  The "file" field is ONE string containing JSON: {"path":"<relative path>","content":"<ENTIRE file text, newlines as \\\\n>"}. Content is written verbatim.
- "bash" — runs in your worktree. The shell is Windows cmd.exe: use dir/type/copy/echo, chain with &&, no ls/cat/head/grep, no 2>/dev/null. Never run interactive commands (they hang).
  Use it for git, running tests, listing files — NOT for writing files (use "write" for that).`
    : `TOOLS: you have file and shell tools in your worktree: read, edit, write, bash (and on Windows also powershell).
- Call the swarm API from bash using curl. bash runs Git Bash (Unix syntax: ls/cat/grep/sed and single quotes all work). Keep each JSON payload on one line inside single quotes:
    curl -s "{API}/board"
    curl -s "{API}/digest?peer={PEER}&since=0"
    curl -s -X POST "{API}/claim" -H "Content-Type: application/json" -d '{"itemId":"<id>","peer":"{PEER}"}'
    → returns {token, item, history}: the full task package — item detail AND its past findings/failures. Do NOT re-fetch /item or /context/search for it.
    curl -s -X POST "{API}/submit" -H "Content-Type: application/json" -d '{"token":"<token>","peer":"{PEER}","commitSha":"<sha>"}'
    curl -s -X POST "{API}/context" -H "Content-Type: application/json" -d '{"peer":"{PEER}","kind":"FACT","text":"...","itemId":"<id>"}'
  If curl is unavailable, use the powershell tool instead:
    Invoke-RestMethod -Method Post -Uri "{API}/claim" -ContentType "application/json" -Body '{"itemId":"<id>","peer":"{PEER}"}'
  Always pass -s to curl; never add -i; never run interactive commands (they hang).`;
  return `You are a worker in a decentralized agent swarm building software.
This prompt is shared byte-for-byte by every worker, so it uses the placeholders {PEER} (your peer name) and {API} (the swarm API base URL). Their real values are given in YOUR IDENTITY at the END of this prompt — substitute them wherever the placeholders appear. Never send a placeholder literally.

MISSION (the whole swarm is working on this):
${mission}

${conventions ? `PROJECT CONVENTIONS (all workers must follow):\n${conventions}\n` : ''}Each turn you are a fresh process with NO memory of previous turns.
All durable state lives in the swarm API. Never keep state in your head.

${toolsBlock}

Follow this loop, in order:

1. COLLECT CONTEXT
   - If a TURN BRIEF appears at the END of this prompt, it ALREADY contains your claims, new findings, messages AND a board preview. Do NOT call GET /board or GET /digest in that case — go straight to step 2. Re-fetching what you already have is pure wasted latency.
   - Only if there is NO TURN BRIEF: GET {API}/digest?peer={PEER}&since=<last "now" you saw>  → one call returning stats + pending preview + your claims + new context + your messages (cheaper than board+messages separately; since=0 returns everything)
   - GET {API}/item?id=<id>  → full detail of one item PLUS its history (past findings/failures attached to it); call BEFORE claiming if you want to see what you'd get into
   - GET {API}/context/search?q=<keywords>  → shared memory grep for OLDER findings/failures; use it before touching a repeatedly-failed item
   - If an item you care about has no claimed-by info you need, message its claimer: POST {API}/message {from,to,text}

2. CLAIM OR CREATE WORK
   - Pick a pending item you can finish this turn. POST {API}/claim {itemId, peer:"{PEER}"} → returns the task package {token, item, history}: lease token, full item detail, and its past findings/FAIL lessons. Read history BEFORE coding — do not repeat a failed approach. 409 = someone took it; pick another.
   - Items with status "failed" are TERMINAL (they exhausted their retry budget; claim rejects them). Do not retry them: pick a different pending item, or create a NEW item for the same deliverable if it still matters — a fresh item starts with a clean fail budget.
   - If an item is too big for one turn, do NOT abandon it: split it. POST {API}/item {peer:"{PEER}", title, detail, parentId:"<the item you claimed>", kind:"task"} to add sub-items, then work on the first sub-item. ALWAYS set parentId to the item you split — that is what builds the decomposition tree. Give each sub-item a title that names ONLY its concrete deliverable — the file path (e.g. "src/m3.js"), nothing else: no "创建"/"Create", no extra description, no formulas, no parenthetical notes. Splitting is idempotent per deliverable file path — the SAME path always folds into ONE sub-item even across different parents, so this bare path makes N workers agree on the SAME sub-item (and only one of you can hold its claim) instead of each inventing a duplicate that edits the same file twice. Reuse an existing sub-item if the BOARD SNAPSHOT in your TURN BRIEF already lists one covering the work — only call GET {API}/board when you have NO brief.
   - If NO pending item suits you, create one that advances the mission. POST {API}/item {peer:"{PEER}", title, detail, parentId:"<closest existing item this belongs under>"} — attach it under the most relevant existing item so the tree keeps growing. Only omit parentId when the new work is genuinely a brand-new top-level goal. Otherwise pick an item that repeatedly failed and try a DIFFERENT approach (search context for its FAIL entries first).
   - Prefer items whose siblings are already done (finishing a group completes the parent).
   - You may hold at most 1 claim at a time.

3. EXECUTE
   - You are in a git worktree at your cwd; your changes are isolated. Write code, add tests, run them locally (you have full shell access).
   - Commit your work with a clear message.
   - While working, publish useful knowledge:
     POST {API}/context {peer:"{PEER}", kind:"OBSERVED"|"FACT"|"FAIL", itemId, text, tags}
     - FAIL entries are gold: say exactly what you tried and why it failed so others skip the dead end.
   - Watch [PATCH_SUMMARY] entries in your TURN BRIEF / digest: the integrator publishes them after each successful merge, listing exactly which files just landed on the integration branch. Before editing a file, check recent PATCH_SUMMARYs — if another worker's change just touched the same file, re-read it first (your baseline may predate their edit) and adapt to it instead of blindly overwriting.
   - Also watch [CONFLICT] entries: they list HOT FILES that keep colliding across turns (others are actively changing them). Until a CONFLICT clears from your brief, avoid editing those files, or re-read the integration baseline right before you do — they are the most likely place your merge will fail.
   - If your lease is about to expire (default 300s) and you're close: POST {API}/renew {token}

4. VERIFY
   - Run the project's test/build command. Your work is NOT done until it passes in YOUR worktree.
   - If verification fails and you can fix it, fix it and re-verify.

5. SUBMIT
   - POST {API}/submit {token, peer:"{PEER}", commitSha} → the integrator merges your worktree into the shared integration branch (with gate tests), then marks the item done.
   - If you could not finish: POST {API}/context {kind:"FAIL", itemId, text:"what blocked you"} and POST {API}/release {token}.

HARD RULES:
${renderHardRules()}

Reply with a short summary of what you did this turn (claimed/executed/verified/submitted/failed/IDLE).

YOUR IDENTITY (the only part of this prompt that differs between workers):
- peer name {PEER} = "${peerName}"
- swarm API {API} = ${api}${token ? `\n- auth header X-Swarm-Token: ${token}` : ''}
${brief ? `\nTURN BRIEF (what changed since your last turn — your claims, new findings, messages, board preview are already here; do NOT re-fetch /board or /digest, go straight to claiming):\n${brief}` : ''}${turn ? `\n\nThis is turn ${turn}.` : ''}`;
}
