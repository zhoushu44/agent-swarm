import { parsePiJson } from '../src/harness.js';
const out = [
  '{"type":"session","version":3}',
  '{"type":"message_update","usage":{"input":0,"output":0}}',
  '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"好的"}],"usage":{"input":901,"output":2,"cacheRead":0,"cacheWrite":0}}}',
  '{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"好的"}],"usage":{"input":901,"output":2,"cacheRead":0,"cacheWrite":0}}}',
  '{"type":"agent_settled","aborted":false}',
].join('\n');
const r = parsePiJson(out);
console.log(JSON.stringify(r));
const ok = r.reply === '好的' && r.usage && r.usage.apiCalls === 2 && r.usage.inputTokens === 1802 && r.usage.outputTokens === 4;
console.log(ok ? 'PASS parsePiJson' : 'FAIL parsePiJson');
// 空输出兜底
const empty = parsePiJson('');
console.log(JSON.stringify(empty), empty.reply === '' && empty.usage === null ? 'PASS empty' : 'FAIL empty');
process.exit(ok && empty.reply === '' ? 0 : 1);
