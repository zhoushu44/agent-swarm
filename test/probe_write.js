// 探针：验证中转站桥能否完整带回一个"长多行字符串"工具参数。
// 背景：worker 写代码文件时把内容当 shell 参数传给 cmd.exe 会被截断（实测 "only 49 bytes landed"），
// 因此打算加一个不走 shell 的 write 工具，入参沿用 http 工具已验证可行的"单字符串装 JSON"模式。
// 本探针确认：模型能否稳定产出该 JSON，且中转站不会丢弃/截断其中的多行 content。
// 注：中转站鉴权/路由存在间歇 401，故这里带重试，便于区分"抖动"与"结构性失败"。
import { loadDirectConfig } from '../src/harness.js';
// 凭据一律走统一 loader，绝不硬编码（缺口⑥ 明文 API key）。
const CFG = loadDirectConfig({ base: process.env.SWARM_API_BASE, token: process.env.SWARM_API_KEY });
const API_BASE = CFG.base;
const API_KEY = CFG.token;
const MODEL = CFG.model;
const url = `${API_BASE.replace(/\/+$/, '')}/messages`;

const tools = [
  { name: 'write', description: 'Create or overwrite a file in your worktree WITHOUT the shell (cmd.exe mangles multi-line content and quotes). ' +
      'Pass ONE string field "file" containing JSON: {"path":"src/m1.js","content":"entire file text"}. Escape newlines as \\n inside the JSON.',
    input_schema: { type: 'object', properties: { file: { type: 'string' } }, required: ['file'] } },
];

const prompt = `Write the file src/m1.js with EXACTLY this content (a JS module, several lines):

// src/m1.js
function f_1(x) {
  return x + 1;
}
module.exports = { f_1 };

Use the write tool.`;

const body = JSON.stringify({ model: MODEL, max_tokens: 2048, tools, tool_choice: { type: 'any' }, messages: [{ role: 'user', content: prompt }] });
const headers = { 'x-api-key': API_KEY, authorization: `Bearer ${API_KEY}`, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' };

let j = null;
for (let attempt = 0; attempt < 4; attempt++) {
  const res = await fetch(url, { method: 'POST', headers, body });
  console.log(`attempt ${attempt}: status = ${res.status}`);
  j = await res.json();
  if (res.ok) break;
  console.log('  body =', JSON.stringify(j).slice(0, 300));
  await new Promise((r) => setTimeout(r, 1000));
}
const tu = (j?.content || []).find((b) => b.type === 'tool_use');
console.log('tool   =', tu?.name);
console.log('raw    =', JSON.stringify(tu?.input));
let parsed = null;
try { parsed = JSON.parse(String(tu?.input?.file || '')); } catch (e) { console.log('JSON PARSE FAILED:', e.message); }
console.log('path   =', parsed?.path);
console.log('bytes  =', parsed?.content ? Buffer.byteLength(parsed.content) : 0);
console.log('lines  =', parsed?.content ? parsed.content.split('\n').length : 0);
console.log('content=', JSON.stringify(parsed?.content));
