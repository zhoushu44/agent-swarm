// 100-agent 压测：真实并发跑 100 个 worker，测编排层的并发能力与瓶颈。
// 关键设计：
//   - 播种 N 个互相独立的小任务（各写一个不同文件），让 100 个 worker 有真实并行工作可做。
//   - 门禁用通用脚本 gate.js（对所有 src/*.js 做语法检查），避免门禁绑定单个 item。
//   - 采集：墙钟、峰值并发、done/total、集成成功率、turn 错误率、事件直方图。
// 用法：node test/stress_100.js   （SWARM_STRESS_N 可调 worker 数）
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
const N = Number(process.env.SWARM_STRESS_N || 100);
const PORT = Number(process.env.SWARM_STRESS_PORT || 17777);
const MAX_MS = Number(process.env.SWARM_STRESS_MAX_MS || 1200000); // 20 分钟上限

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => Date.now();

async function main() {
  console.log(`\n=== stress ${N} agents (model=${MODEL}) ===`);
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-stress-'));
  console.log(`repo: ${repoDir}`);

  const repo = new Repo(repoDir);
  repo.init();
  // 通用门禁：对所有已合并的 src/*.js 做语法检查（--check 同时支持 CJS/ESM 语法）。
  fs.writeFileSync(path.join(repoDir, 'gate.js'), `const fs=require('fs'),path=require('path'),cp=require('child_process');
const dir=path.join(__dirname,'src'); let n=0;
if(fs.existsSync(dir)) for(const f of fs.readdirSync(dir)){ if(f.endsWith('.js')){ cp.execFileSync(process.execPath,['--check',path.join(dir,f)]); n++; } }
console.log('GATE_OK files='+n);
`);
  fs.writeFileSync(path.join(repoDir, 'README.md'), `# stress ${N}\n`);
  execFileSync('git', ['add', '-A'], { cwd: repoDir });
  execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-m', 'init'], { cwd: repoDir });
  // 关键：集成分支必须包含 gate.js，否则集成 worktree 里跑门禁会 "Cannot find module gate.js" 全量失败。
  // repo.init() 在第一次调用时就把集成分支建在了「只含 .gitignore」的初始提交上，这里强制把它对齐到 main。
  execFileSync('git', ['branch', '-f', 'swarm/integration', 'main'], { cwd: repoDir });

  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  // 播种 N 个独立小任务：各写一个不同的 src 文件。
  for (let i = 1; i <= N; i++) {
    store.addItem({
      id: `task${i}`,
      title: `创建 src/m${i}.js`,
      detail: `在仓库里创建 src/m${i}.js，导出一个函数 fn${i}() 返回数字 ${i}。文件必须是合法 JS（CommonJS 或 ESM 均可）。做完后提交。`,
      kind: 'task', createdBy: 'seed', priority: 1,
    });
  }
  console.log(`seeded ${N} independent items`);

  const runner = new Runner({
    store, repoRoot: repoDir, serverPort: PORT,
    harness: 'direct', model: MODEL, apiBase: API_BASE, apiKey: API_KEY,
    mission: `完成 board 上所有待办任务：每个任务创建一个独立的 src 文件。`,
    testCmd: 'node gate.js',
    maxTurns: 4, minWorkers: N, maxWorkers: N, idleTurnsToExit: 2, leaseSec: 600,
    turnTimeoutMs: 240000,
  });
  // 先建 runner 再 createServer：getStatus 闭包引用 runner，压测期间可在
  // http://127.0.0.1:PORT/dashboard 看实时面板。
  const { server } = await createServer(store, { port: PORT, getStatus: () => runner.status() });
  runner.startIntegrator();
  runner.startLeaseRenewer();
  runner.startAutoscaler();
  runner.startReaper();

  const t0 = now();
  // 拉起 N 个 worker（逐个建 worktree，日志报告进度）
  const spawnT0 = now();
  for (let i = 1; i <= N; i++) {
    await runner.spawnWorker(`w${i}`);
    if (i % 25 === 0) console.log(`  spawned ${i}/${N} (${now() - spawnT0}ms)`);
  }
  console.log(`all ${N} workers spawned in ${now() - spawnT0}ms`);

  // 采样循环：每 5s 记录一次，跟踪峰值并发。
  let peakActive = 0;
  let lastLog = 0;
  while (now() - t0 < MAX_MS) {
    const active = [...runner.workers.values()].filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
    peakActive = Math.max(peakActive, active);
    const st = store.stats();
    const el = now() - t0;
    if (el - lastLog >= 10000) {
      lastLog = el;
      console.log(`  [t=${Math.round(el / 1000)}s] active=${active} peak=${peakActive} pending=${st.pending} submitted=${st.submitted} done=${st.done}/${N}`);
    }
    if (st.done + st.pending === 0 && st.submitted === 0 && active === 0) break; // 全部收工
    if (st.done >= N) break;
    await sleep(2000);
  }

  const wall = now() - t0;
  const st = store.stats();
  const ints = store.integrations();
  const evs = store.eventsAfter(0, 100000);
  const types = {};
  for (const e of evs) types[e.type] = (types[e.type] || 0) + 1;

  console.log(`\n=== RESULT ===`);
  console.log(`wall=${(wall / 1000).toFixed(1)}s  peakActive=${peakActive}`);
  console.log(`items: total=${st.total} pending=${st.pending} submitted=${st.submitted} done=${st.done}`);
  console.log(`integrations: n=${ints.length} ok=${ints.filter((i) => i.merged === 1).length}`);
  console.log(`events: ${JSON.stringify(types)}`);
  console.log(`turn_error=${types['worker.turn_error'] || 0} turn_done=${types['worker.turn_done'] || 0} idle=${types['worker.idle'] || 0}`);

  // 集成分支产物统计
  let files = [];
  try {
    files = execFileSync('git', ['ls-tree', '-r', '--name-only', 'swarm/integration'], { cwd: repoDir, encoding: 'utf8' })
      .split('\n').filter((f) => /^src\/m\d+\.js$/.test(f));
  } catch { /* ignore */ }
  console.log(`merged src files: ${files.length}/${N}`);

  console.log('\nstopping...');
  await runner.stop();
  server.close();
  store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
  console.log('done.');
}

main().catch((e) => { console.error(e); process.exit(1); });
