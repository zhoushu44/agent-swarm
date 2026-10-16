// 探针4：找出中转站桥下，哪种 http 工具 schema 能让模型稳定产出完整参数。
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

const p = `Board item "task1" is pending. Claim it as peer "w1" by calling the http tool with method=POST, path=/claim, itemId="task1", peer="w1".`;

// A: 单个 required 字符串字段，装 JSON 文本
const A = [{ name: 'http', description: 'Call the swarm API. Put the FULL JSON request as a string in "request", e.g. {"method":"POST","path":"/claim","itemId":"task1","peer":"w1"}.',
  input_schema: { type: 'object', properties: { request: { type: 'string' } }, required: ['request'] } }];

// B: 扁平 + required 含 itemId/peer
const B = [{ name: 'http', description: 'Call the swarm API. Flat top-level fields only.',
  input_schema: { type: 'object', properties: {
    method: { type: 'string' }, path: { type: 'string' }, itemId: { type: 'string' }, peer: { type: 'string' },
  }, required: ['method', 'path', 'itemId', 'peer'] } }];

// C: 扁平但字段名极简 (m/p/id/who)
const C = [{ name: 'http', description: 'Call the swarm API.',
  input_schema: { type: 'object', properties: {
    method: { type: 'string' }, path: { type: 'string' }, id: { type: 'string' }, who: { type: 'string' },
  }, required: ['method', 'path'] } }];

// D: 扁平，带字段描述
const D = [{ name: 'http', description: 'Call the swarm API. Flat fields.',
  input_schema: { type: 'object', properties: {
    method: { type: 'string', description: 'GET or POST' },
    path: { type: 'string', description: 'e.g. /board, /claim, /submit' },
    itemId: { type: 'string', description: 'the work item id to act on (e.g. task1)' },
    peer: { type: 'string', description: 'your worker name' },
    token: { type: 'string', description: 'lease token if you have one' },
  }, required: ['method', 'path'] } }];

await probe('A single-json-string', A, p);
await probe('B flat-required', B, p);
await probe('C minimal-names', C, p);
await probe('D flat-with-desc', D, p);
