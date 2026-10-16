// 探针：直接看该模型在 tool_choice:any 下返回的 tool_use 原始结构。
import { loadDirectConfig } from '../src/harness.js';
// 凭据一律走统一 loader（env > ~/.swarm/api.json > ~/.claude/settings.json），绝不硬编码（缺口⑥）。
const CFG = loadDirectConfig({ base: process.env.SWARM_API_BASE, token: process.env.SWARM_API_KEY });
const API_BASE = CFG.base;
const API_KEY = CFG.token;
const MODEL = CFG.model;

const tools = [
  { name: 'http', description: 'Call the swarm API. Pass POST payload as the "body" object.',
    input_schema: { type: 'object', properties: { method: { type: 'string' }, path: { type: 'string' }, body: { type: 'object' } }, required: ['method', 'path'] } },
];

const prompt = `You are worker "w1". A board item with id "task1" is pending.
Claim it by calling the http tool: POST /claim with body {"itemId":"task1","peer":"w1"}.`;

const res = await fetch(`${API_BASE.replace(/\/+$/, '')}/messages`, {
  method: 'POST',
  headers: { 'x-api-key': API_KEY, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
  body: JSON.stringify({ model: MODEL, max_tokens: 1024, tools, tool_choice: { type: 'any' }, messages: [{ role: 'user', content: prompt }] }),
});
console.log('status', res.status);
const j = await res.json();
console.log(JSON.stringify(j, null, 2).slice(0, 3000));
