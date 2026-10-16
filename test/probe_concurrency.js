// 临时探针：测中转站在 1/10/50/100 并发下的成功率与延迟。
// 目的：100 agent 压测前先确认中转站不会限流——否则压测结果全是"API 429"，无意义还烧钱。
import { loadDirectConfig } from '../src/harness.js';
// 凭据一律走统一 loader，绝不硬编码（缺口⑥ 明文 API key）。
const CFG = loadDirectConfig({ base: process.env.SWARM_API_BASE, token: process.env.SWARM_API_KEY });
const BASE = CFG.base;
const KEY = CFG.token;
const MODEL = CFG.model;
const url = BASE.replace(/\/+$/, '') + '/messages';

const tools = [
  { name: 'bash', description: 'Run a shell command.', input_schema: { type: 'object', properties: { cmd: { type: 'string' } }, required: ['cmd'] } },
  { name: 'finish', description: 'End turn.', input_schema: { type: 'object', properties: { summary: { type: 'string' } }, required: ['summary'] } },
];

// 贴近真实 worker 的一步：带工具 + tool_choice:any，prompt 有实际体量。
const prompt = `You are worker wN in a decentralized agent swarm. MISSION: create src/math.js exporting add(a,b).
Step 1: use bash to write the file. Then call finish with a summary. Keep it short.`;

async function one(i) {
  const t0 = Date.now();
  try {
    const res = await fetch(url, {
      method: 'POST',
      headers: { 'x-api-key': KEY, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
      body: JSON.stringify({ model: MODEL, max_tokens: 512, tools, tool_choice: { type: 'any' }, messages: [{ role: 'user', content: prompt }] }),
    });
    const ms = Date.now() - t0;
    const j = await res.json().catch(() => ({}));
    const ok = res.ok && (j.content || []).some((b) => b.type === 'tool_use');
    return { ok, status: res.status, ms, err: res.ok ? null : (j?.error?.message || JSON.stringify(j).slice(0, 120)) };
  } catch (e) {
    return { ok: false, status: 0, ms: Date.now() - t0, err: String(e && e.message || e).slice(0, 120) };
  }
}

async function level(n) {
  const t0 = Date.now();
  const rs = await Promise.all(Array.from({ length: n }, (_, i) => one(i)));
  const wall = Date.now() - t0;
  const ok = rs.filter((r) => r.ok).length;
  const ms = rs.map((r) => r.ms).sort((a, b) => a - b);
  const p50 = ms[Math.floor(ms.length * 0.5)] || 0;
  const p95 = ms[Math.floor(ms.length * 0.95)] || 0;
  const errs = {};
  for (const r of rs) if (!r.ok) { const k = `${r.status} ${r.err || ''}`.slice(0, 60); errs[k] = (errs[k] || 0) + 1; }
  console.log(`concurrency=${String(n).padStart(3)}  ok=${ok}/${n}  wall=${wall}ms  p50=${p50}ms p95=${p95}ms  ${ok < n ? 'ERR=' + JSON.stringify(errs) : ''}`);
  return { n, ok, wall };
}

(async () => {
  console.log(`probe ${url} model=${MODEL}`);
  for (const n of [1, 10, 50, 100]) { await level(n); }
})();
