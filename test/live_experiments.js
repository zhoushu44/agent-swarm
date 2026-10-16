// 三个真实 API 实验（不 mock）：
//   EXP1 运行中发现问题 —— 门禁失败 → 发布 FAIL 上下文 → item 退回 pending → 下一个认领者看到 FAIL 并换策略
//   EXP2 运行中省 token —— "回合简报"开/关 A/B，对照真实 input_tokens / API 往返次数
//   EXP3 速度对比 —— 同一批任务：单 agent（1 worker）串行 vs swarm（4 worker）并行的墙钟时间
//   EXP4 单任务并行 —— 只有【一个】大任务，靠 worker 自己 split 拆子项：单 agent（1 worker） vs swarm（10 worker）
//
// 用法：node test/live_experiments.js [1|2|3|4]   （不带参数跑全部）
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { Store } from '../src/store.js';
import { Repo } from '../src/gitutil.js';
import { createServer } from '../src/server.js';
import { Runner } from '../src/runner.js';
import { loadDirectConfig } from '../src/harness.js';

// 凭据一律走统一 loader（env > ~/.swarm/api.json > ~/.claude/settings.json），
// 绝不硬编码——曾把真实 key 明文写进测试脚本（缺口⑥ 明文 API key）。
const CFG = loadDirectConfig({ base: process.env.SWARM_API_BASE, token: process.env.SWARM_API_KEY });
const API_BASE = CFG.base;
const API_KEY = CFG.token;
const MODEL = CFG.model;

const results = [];
function check(name, ok, extra = '') {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? '  — ' + extra : ''}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// Windows 下 git worktree 的文件句柄可能尚未释放，rmSync 会 EPERM；清理失败不该让整个实验崩掉。
function safeRm(dir) {
  for (let i = 0; i < 5; i++) {
    try { fs.rmSync(dir, { recursive: true, force: true }); return; }
    catch { execFileSync('git', ['worktree', 'prune'], { cwd: dir, stdio: 'ignore' }); }
  }
  try { fs.rmSync(dir, { recursive: true, force: true, maxRetries: 3 }); } catch { /* 放弃 */ }
}
async function waitFor(fn, { label, timeoutMs = 300000, interval = 2000 } = {}) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) { try { if (fn()) return true; } catch { /* 忽略 */ } await sleep(interval); }
  console.log(`  (waitFor 超时: ${label})`);
  return false;
}

// ---------- 公共脚手架 ----------
function tmpRepo(tag) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `swarm-${tag}-`));
  const repo = new Repo(dir);
  repo.init();
  fs.writeFileSync(path.join(dir, 'README.md'), `# ${tag}\n`);
  fs.writeFileSync(path.join(dir, '.gitignore'), '.swarm/\n');
  execFileSync('git', ['add', '-A'], { cwd: dir });
  execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-m', 'init'], { cwd: dir });
  repo.init();
  return dir;
}
// 门禁脚本写在仓库之外：worker 看不到它，因此"陷阱"不会被提前识破（实验 1 的确定性来源）。
function writeGate(tag, body) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `swarm-gate-${tag}-`));
  const p = path.join(dir, 'gate.js');
  fs.writeFileSync(p, body);
  return `node "${p}"`;
}

async function launch({ port, workerCount, maxTurns, briefEnabled, repoDir, mission, testCmd, turnTimeoutMs = 240000, idleTurnsToExit = 2, leaseSec = 300 }) {
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  const { server } = await createServer(store, { port });
  const runner = new Runner({
    store, repoRoot: repoDir, serverPort: port, harness: 'direct', model: MODEL,
    apiBase: API_BASE, apiKey: API_KEY, mission, testCmd,
    maxTurns, minWorkers: workerCount, maxWorkers: workerCount,
    idleTurnsToExit, leaseSec, turnTimeoutMs, briefEnabled,
  });
  runner.startIntegrator();
  runner.startLeaseRenewer();
  runner.startReaper();
  const names = [];
  for (let i = 0; i < workerCount; i++) { const n = `w${i + 1}`; await runner.spawnWorker(n); names.push(n); }
  return { store, server, runner, names };
}

function eventTypes(store) {
  const t = {};
  for (const e of store.eventsAfter(0, 100000)) t[e.type] = (t[e.type] || 0) + 1;
  return t;
}
function tokenLine(u) {
  // stepsPerCall：服务端往返延迟是墙钟主导项，故"平均每 turn 消耗几步"是优化效果的稳定度量
  // （墙钟被服务端波动主导，同一任务实测 99s~313s，不可比）。
  const spc = u.turns ? (u.steps / u.turns).toFixed(1) : '-';
  return `apiCalls=${u.apiCalls} steps=${u.steps || 0} steps/turn=${spc} in=${u.inputTokens} out=${u.outputTokens} cacheRead=${u.cacheReadTokens}`;
}
// 时间线诊断：从事件日志重建各阶段耗时，定位墙钟时间的真正去处。
// 阶段划分（相对 t0）：
//   coldStart = 第一个子项被创建的时刻（在此之前只有 1 个 worker 在干活，其余空转）
//   splitDone = 最后一个子项被创建的时刻（拆解完全就绪，并行度上限才被打开）
//   firstChildClaim = 第一个子项被认领的时刻（并行真正开始）
//   lastSubmit = 最后一次提交时刻（worker 侧全部干完）
//   lastIntegrate = 最后一次集成成功时刻（含串行门禁）
function printTimeline(store, t0, tag) {
  const ev = store.eventsAfter(0, 200000);
  const rel = (ts) => ((ts - t0) / 1000).toFixed(1) + 's';
  const first = (type, pred) => ev.find((e) => e.type === type && (!pred || pred(e.payload)));
  const last = (type, pred) => [...ev].reverse().find((e) => e.type === type && (!pred || pred(e.payload)));
  const childIds = new Set(ev.filter((e) => e.type === 'item.added' && e.payload.parentId).map((e) => e.payload.id));
  const firstChildAdd = ev.find((e) => e.type === 'item.added' && e.payload.parentId);
  const lastChildAdd = last('item.added', (p) => p.parentId);
  const firstChildClaim = first('item.claimed', (p) => childIds.has(p.itemId));
  const lastSubmit = last('item.submitted');
  const lastInt = last('integration.ok');
  const idles = ev.filter((e) => e.type === 'worker.idle').length;
  const turnDone = ev.filter((e) => e.type === 'worker.turn_done').length;
  const parts = [
    `coldStart(首个拆解)=${firstChildAdd ? rel(firstChildAdd.ts) : 'n/a'}`,
    `splitDone(拆解完成)=${lastChildAdd ? rel(lastChildAdd.ts) : 'n/a'}`,
    `firstChildClaim(并行开始)=${firstChildClaim ? rel(firstChildClaim.ts) : 'n/a'}`,
    `lastSubmit(干完)=${lastSubmit ? rel(lastSubmit.ts) : 'n/a'}`,
    `lastIntegrate(集成完成)=${lastInt ? rel(lastInt.ts) : 'n/a'}`,
  ];
  console.log(`  [${tag}] 时间线: ${parts.join('  ')}`);
  console.log(`  [${tag}] 事件: turns=${turnDone} idles=${idles} submissions=${ev.filter((e) => e.type === 'item.submitted').length} integrations=${ev.filter((e) => e.type === 'integration.ok').length}`);
  // 逐条调度明细：每个 worker 的认领/提交时刻，暴露"是否真并行"。
  if (process.env.SWARM_TIMELINE_DETAIL) {
    const rows = ev.filter((e) => ['item.claimed', 'item.submitted', 'worker.idle', 'worker.turn_done', 'integration.ok', 'worker.spawned'].includes(e.type))
      .map((e) => `    +${rel(e.ts).padStart(7)}  ${e.type.padEnd(18)} ${e.peer.padEnd(5)} ${JSON.stringify(e.payload).slice(0, 90)}`);
    console.log(rows.join('\n'));
  }
}

// ================= EXP1：运行中发现问题 =================
// 陷阱设计：item 让 agent 建 src/temp.js；项目门禁（仓库外脚本）禁止该文件名，并提示改名 src/math.js。
// 第一个提交者必然撞门禁 → FAIL 上下文落到该 item 的历史里 → 退回 pending → 下一个认领者在 /claim 的 history 里读到 FAIL。
export async function exp1() {
  console.log('\n=== EXP1 运行中发现问题（门禁失败闭环） ===');
  const repoDir = tmpRepo('exp1');
  const testCmd = writeGate('exp1', `
const fs = require('fs');
if (fs.existsSync('src/temp.js')) {
  console.error('FAIL: src/temp.js is a forbidden scratch filename (project policy). Rename it to src/math.js.');
  process.exit(1);
}
if (!fs.existsSync('src/math.js')) { console.error('FAIL: src/math.js is missing.'); process.exit(1); }
console.log('OK');
process.exit(0);
`);
  const PORT = Number(process.env.SWARM_EXP1_PORT || 17880);
  const { store, server, runner } = await launch({
    port: PORT, workerCount: 2, maxTurns: 4, briefEnabled: true, repoDir, testCmd,
    mission: '在仓库里创建 src/math.js，导出 add(a,b) 函数并写一个能跑通的测试。',
    turnTimeoutMs: 240000,
  });
  store.addItem({
    id: 't1', title: '创建 src/temp.js 导出 add(a,b)',
    detail: '请创建 src/temp.js，用 CommonJS 导出 add(a,b) 返回 a+b（可另写一个测试文件）。写完后 commit 并 POST /submit 提交。',
    kind: 'task', createdBy: 'seed', priority: 5,
  });
  const t0 = Date.now();
  // 1) 门禁失败确实发生过
  const failedOnce = await waitFor(() => store.integrations().some((i) => i.merged === 0),
    { label: '第一次门禁失败', timeoutMs: 420000, interval: 3000 });
  // 2) FAIL 上下文已发布（挂在 t1 上）
  const ctxPublished = await waitFor(() => store.stmts.contextByItem.all('t1', 20).some((h) => h.kind === 'FAIL'),
    { label: 'FAIL 上下文已发布', timeoutMs: 120000, interval: 2000 });
  // 3) 退回 pending 并被第二个认领者接手
  const retried = await waitFor(() => {
    const claims = store.eventsAfter(0, 100000).filter((e) => e.type === 'item.claimed' && e.payload?.itemId === 't1').length;
    return claims >= 2 || store.item('t1').status === 'done';
  }, { label: 't1 被再次认领', timeoutMs: 180000, interval: 3000 });
  // 4) 端到端收敛：src/math.js 进入集成分支（给第二个 agent 足够时间读完 FAIL 后换策略）
  const converged = await waitFor(() => {
    let tree = '';
    try { tree = execFileSync('git', ['ls-tree', '-r', '--name-only', 'swarm/integration'], { cwd: repoDir, encoding: 'utf8' }); } catch { tree = ''; }
    return tree.includes('src/math.js') || store.item('t1').status === 'done';
  }, { label: '端到端收敛', timeoutMs: 240000, interval: 3000 });
  const wall = Date.now() - t0;

  const ints = store.integrations();
  const fails = ints.filter((i) => i.merged === 0);
  const hist = store.stmts.contextByItem.all('t1', 20);
  const failRow = hist.find((h) => h.kind === 'FAIL');
  const failsCtx = store.searchContext('forbidden scratch filename', 10);
  const claims = store.eventsAfter(0, 100000).filter((e) => e.type === 'item.claimed' && e.payload?.itemId === 't1').length;
  const backoffs = store.eventsAfter(0, 100000).filter((e) => e.type === 'item.retry_backoff');
  const failEv = store.eventsAfter(0, 100000).find((e) => e.type === 'integration.failed');
  let merged = '';
  try { merged = execFileSync('git', ['ls-tree', '-r', '--name-only', 'swarm/integration'], { cwd: repoDir, encoding: 'utf8' }); } catch { merged = ''; }

  check('门禁确实拦下了第一次提交（merged=0 记录存在）', failedOnce && fails.length >= 1, `failed=${fails.length}`);
  check('集成器发布了 FAIL 上下文（含门禁原始输出）', !!failRow && /forbidden scratch filename|src\/temp\.js/i.test(failRow.text),
    `stage=${failEv?.payload?.stage} text="${String(failRow?.text || '').slice(0, 130)}"`);
  check('FAIL 上下文挂在失败的那个 item 上（认领时随 history 捎带）', hist.some((h) => h.kind === 'FAIL'), `history=${hist.length} kinds=${hist.map((h) => h.kind).join(',')}`);
  check('item 退回 pending 并带退避（retry_backoff 事件）', backoffs.length >= 1, `n=${backoffs.length}`);
  check('失败后 item 被再次认领（下一个 agent 接手）', claims >= 2, `claims=${claims} status=${store.item('t1').status}`);
  check('端到端最终收敛：src/math.js 进入集成分支', converged && merged.includes('src/math.js'), `item=${store.item('t1').status}`);

  console.log(`  观测：wall=${(wall / 1000).toFixed(1)}s  claims(t1)=${claims}  集成次数=${ints.length}(失败${fails.length})  ${tokenLine(runner.tokenUsage)}`);
  console.log(`  事件类型：${JSON.stringify(eventTypes(store))}`);

  await runner.stop(); server.close(); store.close();
  safeRm(repoDir);
}

// ================= EXP2：运行中省 token =================
// 2a 确定性测量：同一份 board 状态，"回合简报"载荷 vs "全量 digest"载荷的字符量对比。
// 2b 真实 A/B：同一批任务、同样 2 个 worker，briefEnabled=true vs false，对照真实 input_tokens 与 API 往返次数。
function seedCommon(store, n) {
  for (let i = 1; i <= n; i++) {
    store.addItem({
      id: `m${i}`, title: `创建 src/m${i}.js 导出 f${i}(x)`,
      detail: `创建 src/m${i}.js，用 CommonJS 导出 f${i}(x) 返回 x*${i}。写完 commit 并 POST /submit 提交。`,
      kind: 'task', createdBy: 'seed', priority: 5 - i,
    });
  }
  // 预置共享上下文/消息：让简报有内容可捎带（否则省 token 实验测的是空载荷）
  const kinds = ['FACT', 'OBSERVED', 'FAIL'];
  for (let i = 0; i < 8; i++) {
    store.addContext('w9', kinds[i % 3], `预置共享知识条目 #${i}：项目约定、已知坑点与历史失败教训的说明文本，用于制造真实的上下文载荷。`, { itemId: `m${(i % n) + 1}`, tags: ['seed'] });
  }
  store.postMessage('w9', 'w1', '预置消息：请注意 src 目录下的文件命名规范，避免使用临时文件名。', 'm1');
  store.postMessage('w9', 'w1', '预置消息：如果门禁失败，请先读 item 的历史 FAIL 再动手。', 'm2');
}

async function exp2a() {
  console.log('\n--- EXP2a 简报载荷 vs 全量 digest 载荷（确定性，不调 API） ---');
  const repoDir = tmpRepo('exp2a');
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  seedCommon(store, 5);
  const runner = new Runner({ store, repoRoot: repoDir, serverPort: 0, harness: 'echo', mission: 'x', testCmd: null });
  const w = { name: 'w1', lastReply: 'claimed m1, wrote src/m1.js, submitted.' };
  const rows = [];
  for (let turn = 1; turn <= 3; turn++) {
    const d = store.digestFor('w1', 0, 30);
    const briefChars = runner._buildBrief(w, d).length;
    const digestChars = JSON.stringify(d).length;
    rows.push({ turn, briefChars, digestChars });
    // 模拟后续回合新产生的共享知识，让载荷随规模增长
    store.addContext('w2', 'OBSERVED', `回合 ${turn} 新增的共享观察：这是一条足够长的说明文本，用来模拟真实运行中不断增长的共享上下文。`, { itemId: 'm3' });
    store.postMessage('w2', 'w1', `回合 ${turn} 的消息：某文件已被集成，请注意基线刷新。`, 'm3');
  }
  let briefTotal = 0, digestTotal = 0;
  for (const r of rows) {
    briefTotal += r.briefChars; digestTotal += r.digestChars;
    console.log(`  turn${r.turn}: brief=${r.briefChars} chars  fullDigest=${r.digestChars} chars  省=${(100 - r.briefChars / r.digestChars * 100).toFixed(1)}%`);
  }
  const saved = 100 - briefTotal / digestTotal * 100;
  console.log(`  3 回合合计：brief=${briefTotal}  fullDigest=${digestTotal}  省 ${saved.toFixed(1)}%（约 ${Math.round((digestTotal - briefTotal) / 4)} tokens 的字符差）`);
  check('简报载荷显著小于全量 digest 载荷（>30%）', saved > 30, `saved=${saved.toFixed(1)}%`);
  store.close();
  safeRm(repoDir);
}

async function exp2b(briefEnabled, port, tag) {
  const repoDir = tmpRepo(tag);
  const testCmd = writeGate(tag, `
const fs = require('fs'), path = require('path'), { execFileSync } = require('child_process');
let n = 0;
for (const f of fs.readdirSync('src')) {
  if (!f.endsWith('.js')) continue;
  execFileSync(process.execPath, ['--check', path.join('src', f)]);
  n++;
}
if (n < 1) { console.error('FAIL: no src/*.js produced'); process.exit(1); }
console.log('OK ' + n);
`);
  const { store, server, runner } = await launch({
    port, workerCount: 2, maxTurns: 3, briefEnabled, repoDir, testCmd,
    mission: '在仓库里创建 5 个独立的小模块 src/m1.js..src/m5.js，每个导出 fi(x)=x*i，并提交。',
  });
  seedCommon(store, 5);
  const t0 = Date.now();
  const done = await waitFor(() => store.stats().done >= 5, { label: `brief=${briefEnabled} 全部完成`, timeoutMs: 480000, interval: 3000 });
  const wall = Date.now() - t0;
  const evs = store.eventsAfter(0, 100000);
  const turns = evs.filter((e) => e.type === 'worker.turn_done');
  const promptChars = turns.reduce((s, e) => s + (e.payload?.promptChars || 0), 0);
  const avgPrompt = turns.length ? Math.round(promptChars / turns.length) : 0;
  console.log(`  brief=${briefEnabled}: done=${store.stats().done}/5 wall=${(wall / 1000).toFixed(1)}s turns=${turns.length} avgPromptChars=${avgPrompt} ${tokenLine(runner.tokenUsage)}`);
  const out = { wall, done: store.stats().done, turns: turns.length, avgPrompt, usage: { ...runner.tokenUsage }, digests: evs.filter((e) => e.type === 'worker.turn_done' && String(e.payload?.reply || '').includes('digest')).length };
  await runner.stop(); server.close(); store.close();
  safeRm(repoDir);
  return out;
}

export async function exp2() {
  console.log('\n=== EXP2 运行中省 token（回合简报 A/B） ===');
  await exp2a();
  console.log('\n--- EXP2b 真实 A/B（同一批任务、同样 2 worker） ---');
  const on = await exp2b(true, Number(process.env.SWARM_EXP2A_PORT || 17881), 'exp2on');
  const off = await exp2b(false, Number(process.env.SWARM_EXP2B_PORT || 17882), 'exp2off');
  const dIn = off.usage.inputTokens - on.usage.inputTokens;
  const pct = off.usage.inputTokens ? (dIn / off.usage.inputTokens * 100) : 0;
  const dCalls = off.usage.apiCalls - on.usage.apiCalls;
  console.log(`  对照：brief 开 in=${on.usage.inputTokens} calls=${on.usage.apiCalls} | brief 关 in=${off.usage.inputTokens} calls=${off.usage.apiCalls}`);
  console.log(`  差值：input_tokens ${dIn >= 0 ? '省' : '多'} ${Math.abs(dIn)}（${Math.abs(pct).toFixed(1)}%）  api 往返 ${dCalls >= 0 ? '少' : '多'} ${Math.abs(dCalls)}`);
  check('简报开启后 API 往返次数不高于关闭时', on.usage.apiCalls <= off.usage.apiCalls, `on=${on.usage.apiCalls} off=${off.usage.apiCalls}`);
  check('简报开启后 input_tokens 不高于关闭时', on.usage.inputTokens <= off.usage.inputTokens, `on=${on.usage.inputTokens} off=${off.usage.inputTokens}`);
  // 机制说明：brief 是把增量"内联"进 prompt，因此平均初始 prompt 反而更长（这是省往返的代价），
  // 省下来的 token 体现在"不再每回合三连拉"→ 总 input_tokens 与 API 往返数下降。
  const inlined = on.avgPrompt - off.avgPrompt;
  check('简报确实被内联进 prompt（省往返机制生效的前提）', inlined > 0, `on=${on.avgPrompt} off=${off.avgPrompt} 差=+${inlined} chars`);
  check('净效果：简报开启后总 input_tokens 严格更低', on.usage.inputTokens < off.usage.inputTokens, `省 ${dIn} tokens`);
  console.log('  注：真实 A/B 的回合数由 agent 行为决定，属观测性结论；2a 的确定性测量是机制层面的证据。');
}

// ================= EXP3：swarm vs 单 agent 速度 =================
function seedBatch(store, n) {
  for (let i = 1; i <= n; i++) {
    store.addItem({
      id: `j${i}`, title: `创建 src/j${i}.js 导出 g${i}(x)`,
      detail: `创建 src/j${i}.js，用 CommonJS 导出 g${i}(x) 返回 x+${i}，文件内附一行注释说明用途。写完 commit 并 POST /submit 提交。`,
      kind: 'task', createdBy: 'seed', priority: 5,
    });
  }
}

async function exp3run(workerCount, port, tag) {
  const N = Number(process.env.SWARM_EXP3_N || 4);
  const repoDir = tmpRepo(tag);
  const testCmd = writeGate(tag, `
const fs = require('fs'), path = require('path'), { execFileSync } = require('child_process');
let n = 0;
for (const f of fs.readdirSync('src')) {
  if (!f.endsWith('.js')) continue;
  execFileSync(process.execPath, ['--check', path.join('src', f)]);
  n++;
}
if (n < 1) { console.error('FAIL: no src/*.js produced'); process.exit(1); }
console.log('OK ' + n);
`);
  const { store, server, runner } = await launch({
    port, workerCount, maxTurns: 5, briefEnabled: true, repoDir, testCmd,
    mission: `在仓库里创建 ${N} 个独立的小模块 src/j1.js..src/j${N}.js，每个导出 gi(x)=x+i，并各自提交。`,
  });
  seedBatch(store, N);
  const t0 = Date.now();
  let peak = 0;
  const tick = setInterval(() => { try { peak = Math.max(peak, runner.status().active); } catch { /* 忽略 */ } }, 1000);
  const allDone = await waitFor(() => {
    for (let i = 1; i <= N; i++) if (store.item(`j${i}`).status !== 'done') return false;
    return true;
  }, { label: `${tag} ${N} 项全部 done`, timeoutMs: 600000, interval: 2000 });
  const wall = Date.now() - t0;
  clearInterval(tick);
  const done = store.stats().done;
  console.log(`  ${tag}（${workerCount} worker）: done=${done}/${N} wall=${(wall / 1000).toFixed(1)}s peakActive=${peak} ${tokenLine(runner.tokenUsage)}`);
  const out = { workerCount, wall, done, peak, allDone, usage: { ...runner.tokenUsage }, turns: store.eventsAfter(0, 100000).filter((e) => e.type === 'worker.turn_done').length };
  await runner.stop(); server.close(); store.close();
  safeRm(repoDir);
  return out;
}

export async function exp3() {
  console.log('\n=== EXP3 速度对比：单 agent（1 worker） vs swarm（4 worker） ===');
  const single = await exp3run(1, Number(process.env.SWARM_EXP3_SINGLE_PORT || 17883), 'exp3-single');
  const swarm = await exp3run(4, Number(process.env.SWARM_EXP3_SWARM_PORT || 17884), 'exp3-swarm');
  const speedup = swarm.wall ? single.wall / swarm.wall : 0;
  console.log(`  单 agent=${(single.wall / 1000).toFixed(1)}s  swarm=${(swarm.wall / 1000).toFixed(1)}s  提速=${speedup.toFixed(2)}x`);
  check('两者都完成了全部任务（对比有效）', single.allDone && swarm.allDone, `single=${single.done} swarm=${swarm.done}`);
  check('swarm 墙钟时间短于单 agent', swarm.wall < single.wall, `${(single.wall / 1000).toFixed(1)}s → ${(swarm.wall / 1000).toFixed(1)}s`);
  check('swarm 确实并行（峰值活跃 worker > 1）', swarm.peak > 1, `peak=${swarm.peak}`);
  console.log('  注：两者都消耗了真实 API 额度；单 agent 基线 = 同一 mission/同一任务集，仅 1 个 worker 串行执行。');
}

// ================= EXP4：同一个具体任务，10 worker vs 单 agent =================
// 与 EXP3 的关键区别：EXP3 是 N 个互相独立的小任务（天然可并行）；EXP4 只有【一个】大任务，
// 必须靠 worker 自己按 prompt 的 split 机制把大任务拆成子项，才可能用上多个 worker。
// 这验证的是"单任务能否吃到并行度"——也是 200~1000 agent 编排里最关键的一步。
function seedSingle(store, n) {
  store.addItem({
    id: 'big',
    title: `在 src/ 下创建 ${n} 个模块 m1.js..m${n}.js`,
    detail: `这是一个大任务：创建 src/m1.js .. src/m${n}.js，每个文件用 CommonJS 导出 f_i(x) 返回 x+i，文件内附一行注释说明用途。` +
      `单个回合做不完：请先把它拆成 ${n} 个子项（POST /item，parentId 设为 "big"，每个子项恰好负责一个模块），然后自己认领其中一个子项完成并提交。` +
      `子项 title 必须写成确定性的文件名，例如 "src/m3.js" —— 不要写"模块3"这类模糊标题：` +
      `拆解按交付物文件名幂等去重（跨父项也折叠），确定性标题才能让所有 worker 认领到【同一批】子项，而不是各建一套重复子项。` +
      `其余子项由其他 worker 并行认领。全部子项完成后父任务会自动 done。` +
      `重要：不要直接提交这个大任务本身——只提交子项，让父任务靠级联自动完成。`,
    kind: 'task', createdBy: 'seed', priority: 5,
  });
}

async function exp4run(workerCount, port, tag, n) {
  const repoDir = tmpRepo(tag);
  // 门禁在【每一次提交】上运行（集成器对每个子项都跑一遍），所以它不能要求"全部 n 个模块都在"——
  // 否则第 1 个子项提交时必然失败 → 退回 pending → 被反复重认领 → 死循环。
  // 正确口径：本次提交至少要落下一个语法合法的模块；最终"是否 n 个模块齐全"由实验尾部单独校验。
  const testCmd = writeGate(tag, `
const fs = require('fs'), path = require('path'), { execFileSync } = require('child_process');
if (!fs.existsSync('src')) { console.error('FAIL: no src/ dir in this submission'); process.exit(1); }
let mods = 0;
for (const f of fs.readdirSync('src')) {
  if (!f.endsWith('.js')) continue;
  execFileSync(process.execPath, ['--check', path.join('src', f)]);
  if (/^m\\d+\\.js$/.test(f)) mods++;
}
if (mods < 1) { console.error('FAIL: this submission produced no module'); process.exit(1); }
console.log('OK modules=' + mods);
`);
  // 两个 run 必须面对【完全同一个任务】——同一 mission、同一门禁、同一初始 board。
  const mission =
    `仓库里只有一个大任务（item id = "big"）：创建 ${n} 个模块 src/m1.js .. src/m${n}.js（每个导出 f_i(x)=x+i）。` +
    `单个回合做不完，必须先把大任务拆成 ${n} 个子项（POST /item，parentId="big"，每个子项恰好负责一个模块，` +
    `title 用确定性的文件名如 "src/m3.js"），再让每个 worker 认领不同的子项并行完成并提交。` +
    `拆解按 (parent, title) 幂等去重：写同一个 title 不会重复建项，所以请用确定性标题让所有 worker 认领同一批子项。` +
    `重要：不要提交 "big" 本身——它靠子项全部完成后自动 done。若暂时认领不到子项，就稍等重试，不要另建无关任务。`;
  // maxTurns/idleTurnsToExit 放大：本实验用固定 worker 池（不启动 autoscaler），
  // 一旦 worker 退场就不会被重建。让它保持轮询，等子项被拆出来后立刻接上。
  const { store, server, runner } = await launch({
    port, workerCount, maxTurns: 40, idleTurnsToExit: 100, briefEnabled: true, repoDir, testCmd, mission,
  });
  seedSingle(store, n);
  const t0 = Date.now();
  let peak = 0;
  const tick = setInterval(() => { try { peak = Math.max(peak, runner.status().active); } catch { /* 忽略 */ } }, 1000);
  const timeoutMs = Number(process.env.SWARM_EXP4_TIMEOUT_MS || 1200000);
  // 交付物落在【集成分支的 worktree】里（repoDir/.swarm/integration），而不是 repoDir 本身——
  // worker 的提交经集成器 merge 到集成分支，主工作区始终保持不动。
  // 旧版错查 repoDir/src 导致条件永不成立、必然空等整个超时（用户看到的"半小时"）。
  const itSrc = path.join(repoDir, '.swarm', 'integration', 'src');
  const countModules = () => (fs.existsSync(itSrc) ? fs.readdirSync(itSrc).filter((f) => /^m\d+\.js$/.test(f)).length : 0);
  // 自建轮询（不用 waitFor）以便每 30s 打印进度——否则长任务会长时间静默，看起来像卡死。
  const deadline = Date.now() + timeoutMs;
  let lastLog = 0;
  const allDone = await (async () => {
    while (Date.now() < deadline) {
      const big = store.item('big');
      if (big && big.status === 'done' && countModules() >= n) return true;
      const now = Date.now();
      if (now - lastLog >= 30000) {
        lastLog = now;
        const st = store.stats();
        console.log(`  [${tag}] ${((now - t0) / 1000).toFixed(0)}s big=${big ? big.status : 'missing'} mods=${countModules()}/${n} ` +
          `open=${store.openCount()} pending=${st.pending} submitted=${st.submitted} done=${st.done} peak=${peak} ${tokenLine(runner.tokenUsage)}`);
      }
      await sleep(3000);
    }
    console.log(`  (waitFor 超时: ${tag} 大任务完成（big done + ${n} 个模块落地）)`);
    return false;
  })();
  const wall = Date.now() - t0;
  clearInterval(tick);
  printTimeline(store, t0, tag);
  const modules = countModules();
  const split = store.recentSplitCount();
  const bigStatus = store.item('big') ? store.item('big').status : 'missing';
  console.log(`  ${tag}（${workerCount} worker）: big=${bigStatus} modules=${modules}/${n} split=${split} wall=${(wall / 1000).toFixed(1)}s peakActive=${peak} ${tokenLine(runner.tokenUsage)}`);
  const out = {
    workerCount, wall, modules, split, peak, allDone, bigStatus,
    usage: { ...runner.tokenUsage },
    turns: store.eventsAfter(0, 100000).filter((e) => e.type === 'worker.turn_done').length,
  };
  await runner.stop(); server.close(); store.close();
  safeRm(repoDir);
  return out;
}

export async function exp4() {
  const N = Number(process.env.SWARM_EXP4_N || 10);
  const W = Number(process.env.SWARM_EXP4_WORKERS || 10);
  console.log(`\n=== EXP4 同一个具体任务：单 agent（1 worker） vs swarm（${W} worker），任务含 ${N} 个模块 ===`);
  const single = await exp4run(1, Number(process.env.SWARM_EXP4_SINGLE_PORT || 17885), 'exp4-single', N);
  const swarm = await exp4run(W, Number(process.env.SWARM_EXP4_SWARM_PORT || 17886), 'exp4-swarm', N);
  const speedup = swarm.wall ? single.wall / swarm.wall : 0;
  console.log(`  单 agent=${(single.wall / 1000).toFixed(1)}s  swarm=${(swarm.wall / 1000).toFixed(1)}s  提速=${speedup.toFixed(2)}x`);
  check('两者都完成了同一个任务（对比有效）', single.allDone && swarm.allDone, `single=${single.modules}/${N} swarm=${swarm.modules}/${N}`);
  check('swarm 墙钟时间短于单 agent', swarm.wall < single.wall, `${(single.wall / 1000).toFixed(1)}s → ${(swarm.wall / 1000).toFixed(1)}s`);
  check('swarm 确实把一个任务拆开并行（峰值活跃 worker > 1）', swarm.peak > 1, `peak=${swarm.peak}`);
  check('swarm 真的发生了任务拆解（产生了子项）', swarm.split > 0, `split=${swarm.split}`);
  console.log(`  注：并行不是免费的 —— token 代价：单 agent ${tokenLine(single.usage)}；swarm ${tokenLine(swarm.usage)}`);
}

// ================= 入口 =================
export async function run(which = null) {
  const sel = which || process.env.SWARM_EXP || null;
  if (!sel || sel === '1') await exp1();
  if (!sel || sel === '2') await exp2();
  if (!sel || sel === '3') await exp3();
  if (!sel || sel === '4') await exp4();
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) { for (const f of failed) console.log(`  FAILED: ${f.name}`); }
  return results;
}

if (import.meta.url === `file://${process.argv[1]}` || process.argv[1]?.endsWith('live_experiments.js')) {
  run(process.argv[2] || null).catch((e) => { console.error(e); process.exit(1); });
}
