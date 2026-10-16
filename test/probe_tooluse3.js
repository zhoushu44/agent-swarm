// 探针3：body 作为 JSON 字符串（string 类型）能否被桥完整带出。
import { loadDirectConfig } from '../src/harness.js';
// 凭据一律走统一 loader，绝不硬编码（缺口⑥）。
const CFG = loadDirectConfig({ base: process.env.SWARM_API_BASE, token: process.env.SWARM_API_KEY });
const API_BASE = CFG.base;
const API_KEY = CFG.token;
const MODEL = CFG.model;
const url = `${API_BASE.replace(/\/+$/, '')}/messages`;

async function probe(label, tools, prompt) {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'x-api-key': API_KEY, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
    body: JSON.stringify({ model: MODEL, max_tokens: 1024, tools, tool_choice: { type: 'any' }, messages: [{ role: 'user', content: prompt }] }),
  });
  const j = await res.json();
  const tu = (j.content || []).find((b) => b.type === 'tool_use');
  console.log(`--- ${label} ---`);
  console.log('input =', JSON.stringify(tu?.input));
}

const strBodyTools = [
  { name: 'http', description: 'Call the swarm API. For POST, put the JSON payload as a STRING in "body".',
    input_schema: { type: 'object', properties: { method: { type: 'string' }, path: { type: 'string' }, body: { type: 'string', description: 'JSON text of the POST payload, e.g. {"itemId":"task1","peer":"w1"}' } }, required: ['method', 'path'] } },
];

const p = `Claim board item "task1" as peer "w1": POST /claim with itemId=task1, peer=w1.`;
await probe('string-body', strBodyTools, p);
