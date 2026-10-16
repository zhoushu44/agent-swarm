// 探针2：对比「嵌套 body」vs「扁平字段」schema，看模型/桥能否带出参数。
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

const flatTools = [
  { name: 'http', description: 'Call the swarm API. Put ALL request fields at the top level of the tool input (no nesting).',
    input_schema: { type: 'object', properties: {
      method: { type: 'string' }, path: { type: 'string' },
      itemId: { type: 'string' }, peer: { type: 'string' }, token: { type: 'string' }, commitSha: { type: 'string' },
    }, required: ['method', 'path'] } },
];

const nestedTools = [
  { name: 'http', description: 'Call the swarm API. Pass POST payload as the "body" object.',
    input_schema: { type: 'object', properties: { method: { type: 'string' }, path: { type: 'string' }, body: { type: 'object' } }, required: ['method', 'path'] } },
];

const p = `Claim board item "task1" as peer "w1": POST /claim with itemId=task1, peer=w1.`;
await probe('flat', flatTools, p);
await probe('nested', nestedTools, p);
