// 真实 harness 端到端测试：direct harness（新中转站 deepseek-v4.1-flash）
// 不依赖任何 mock：真实调 API、真实跑 git worktree、真实集成。
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
function check(name, ok, extra = '') { results.push({ name, ok }); console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? '  — ' + extra : ''}`); }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, { label, timeoutMs = 300000, interval = 2000 } = {}) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) { try { if (fn()) return true; } catch {} await sleep(interval); }
  console.log(`  (waitFor timeout: ${label})`);
  return false;
}

export async function run() {
  console.log(`\n=== live_swarm (direct harness, model=${MODEL}) ===`);
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-live-'));
  const repo = new Repo(repoDir);
  repo.init();
  // 给仓库一个最小可测结构：worker 要写一个能通过的测试
  fs.writeFileSync(path.join(repoDir, 'README.md'), '# live swarm test\n');
  execFileSync('git', ['add', '-A'], { cwd: repoDir });
  execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-m', 'init'], { cwd: repoDir });
  repo.init(); // 补集成分支

  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({
    id: 'task1', title: '创建 math.js 并导出 add(a,b) 函数',
    detail: '在仓库根创建 src/math.js，用 CommonJS 或 ESM 导出 add(a,b) 返回 a+b。然后创建 test_math.js，用 node 运行断言 add(2,3)===5。跑通测试后提交。',
    kind: 'task', createdBy: 'seed', priority: 5,
  });

  const PORT = 17799;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({
    store, repoRoot: repoDir, serverPort: PORT,
    harness: 'direct', model: MODEL, apiBase: API_BASE, apiKey: API_KEY,
    mission: '在仓库里创建 src/math.js 导出 add(a,b)，并写一个能跑通的测试。',
    testCmd: 'node test_math.js',
    maxTurns: 3, minWorkers: 2, maxWorkers: 2, idleTurnsToExit: 2, leaseSec: 600,
    turnTimeoutMs: 300000,
  });
  runner.startIntegrator();
  runner.startLeaseRenewer();
  runner.startAutoscaler();
  runner.startReaper();
  await runner.spawnWorker('w1');
  await runner.spawnWorker('w2');

  // 断言"产物已落地"，而不是"指定的 task1 被认领"。
  // 真实 agent 会自建 item（拆解）再完成等价工作——绑定特定 item id 会假失败。
  const done = await waitFor(() => {
    const st = store.stats();
    if (st.done < 1) return false;
    try { return execFileSync('git', ['ls-tree', '-r', '--name-only', 'swarm/integration'], { cwd: repoDir, encoding: 'utf8' }).includes('src/math.js'); } catch { return false; }
  }, { label: '至少一项 done 且集成分支含 src/math.js', timeoutMs: 600000, interval: 5000 });
  const st = store.stats();
  console.log(`  stats: pending=${st.pending} submitted=${st.submitted} done=${st.done}`);
  check('真实 agent 完成了至少一项工作（done>=1）', st.done >= 1, `done=${st.done}`);
  check('目标产物已进入集成分支', done, `status=${store.item('task1').status}`);

  // 集成分支上应有 math.js
  let files = '';
  try { files = execFileSync('git', ['ls-tree', '-r', '--name-only', 'swarm/integration'], { cwd: repoDir, encoding: 'utf8' }); } catch (e) { files = ''; }
  check('集成分支包含 src/math.js', files.includes('src/math.js'), files.trim().replace(/\n/g, ','));
  check('集成分支包含测试文件', files.includes('test_math.js'), '');

  // 集成记录
  const ints = store.integrations();
  check('存在成功的集成记录', ints.some((i) => i.merged === 1), `n=${ints.length}`);
  // 事件流有真实 turn
  const evs = store.eventsAfter(0, 2000);
  const types = {};
  for (const e of evs) types[e.type] = (types[e.type] || 0) + 1;
  console.log('  event types:', JSON.stringify(types));
  for (const e of evs) {
    const p = e.payload || {};
    if (e.type === 'worker.turn_done') console.log(`    turn_done ${p.name} steps=${p.steps} reply=${String(p.reply || '').slice(0, 120)}`);
    if (e.type === 'worker.turn_error') console.log(`    turn_error ${p.name}: ${String(p.error || '').slice(0, 160)}`);
  }
  check('事件流记录了 worker 生命周期', evs.some((e) => e.type === 'worker.spawned'), `events=${evs.length}`);
  // agent 的真实行动证据：item.claimed/item.submitted 只能来自 agent 自己发的 HTTP 工具调用。
  // 不用 worker.turn_done —— 那是 runner 在"回合返回后"才写的日志，而此刻回合可能仍在飞行中。
  check('事件流记录了 agent 的真实回合', evs.some((e) => e.type === 'item.claimed') && evs.some((e) => e.type === 'item.submitted'), '');

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) throw new Error(`${failed.length} checks failed`);
  return results;
}

if (import.meta.url === `file://${process.argv[1]}` || process.argv[1]?.endsWith('live_swarm_test.js')) {
  run().catch((e) => { console.error(e); process.exit(1); });
}
