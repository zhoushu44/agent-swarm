// 离线协议闭环测试 —— 用 echo harness（零 API 依赖）跑完整链路：
//   worker 认领 → 写码 → 提交 → 集成器门禁 → item done
// 验证点：租约原子性、auto-commit、集成分支合并、门禁通过才 done、stats 正确。
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import assert from 'node:assert';
import { Repo, git } from '../src/gitutil.js';
import { Store } from '../src/store.js';
import { createServer } from '../src/server.js';
import { Runner } from '../src/runner.js';

const PORT = 17899;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function tmpRepo() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-test-'));
  return dir;
}

async function waitFor(fn, { timeoutMs = 30000, intervalMs = 300, label = 'condition' } = {}) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (await fn()) return true;
    await sleep(intervalMs);
  }
  throw new Error(`timeout waiting for ${label}`);
}

export async function run() {
  const repoDir = tmpRepo();
  const results = [];
  const check = (name, cond, extra = '') => {
    results.push({ name, ok: !!cond, extra });
    console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${extra ? '  — ' + extra : ''}`);
    return !!cond;
  };

  // ---------- 1. 初始化仓库 ----------
  const repo = new Repo(repoDir);
  repo.init();
  check('repo init 创建集成分支', !!git(['rev-parse', '--verify', 'swarm/integration'], repoDir));

  // ---------- 2. store + server ----------
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));

  // 租约原子性单测（不经 HTTP）：同一 item 第二次 claim 必须失败
  const it1 = store.addItem({ id: 't1', title: 'build a', createdBy: 'test' });
  const it2 = store.addItem({ id: 't2', title: 'build b', createdBy: 'test' });
  const tok1 = store.tryClaim('t1', 'w1');
  const tok1b = store.tryClaim('t1', 'w2');
  check('tryClaim 原子性：第二个认领返回 null', tok1 && tok1b === null);
  check('checkFence 有效租约返回记录', !!store.checkFence(tok1));

  // 过期的租约不能推进
  const expiredTok = store.tryClaim('t2', 'w2', -1); // 立即过期
  check('过期租约 checkFence 返回 null', expiredTok && store.checkFence(expiredTok) === null);
  store.release(tok1); // 释放 t1 供正式流程使用

  const { server } = await createServer(store, { port: PORT });

  // ---------- 3. Runner（echo harness） ----------
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, apiToken: null,
    harness: 'echo', mission: 'offline test', conventions: null, testCmd: null,
    maxTurns: 4, idleTurnsToExit: 1, maxWorkers: 4, minWorkers: 2,
  });
  // 两个 worker 各领一个 item 的脚本：claim → 写码 → commit → submit
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/a.js module.exports = "a";',
      'commit feat: add a',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say did t1',
    ].join('\n'),
    w2: [
      'claim t2',
      'file src/b.js module.exports = "b";',
      'commit feat: add b',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say did t2',
    ].join('\n'),
  };

  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await runner.spawnWorker('w2');

  // ---------- 4. 等待闭环完成 ----------
  await waitFor(() => store.stats().done >= 2, { label: '2 items done', timeoutMs: 30000 });

  const stats = store.stats();
  check('两个 item 都 done', stats.done === 2, JSON.stringify(stats));

  // item 状态正确
  check('t1 状态 done', store.item('t1').status === 'done');
  check('t2 状态 done', store.item('t2').status === 'done');

  // 集成分支上确实有 worker 的代码
  const files = git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir);
  check('集成分支含 src/a.js', files.includes('src/a.js'), files.split('\n').join(','));
  check('集成分支含 src/b.js', files.includes('src/b.js'));

  // 集成提交带 Agensh-Integration trailer
  const log = git(['log', '--grep=Agensh-Integration:', '-E', '--format=%H %s', 'swarm/integration'], repoDir);
  check('集成分支有 integration trailer 提交', log.trim().length > 0);

  // 集成记录落库
  check('store 有集成记录', store.integrations().length >= 2);
  const merged = store.integrations().filter((i) => i.merged === 1).length;
  check('集成记录标记 merged', merged >= 2, `merged=${merged}`);

  // ---------- 5. 清理 ----------
  await runner.stop(); // 等 worker 循环结束再关 store
  server.close();
  store.close();
  try { fs.rmSync(repoDir, { recursive: true, force: true }); } catch {}

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) throw new Error(`${failed.length} checks failed`);
  return results;
}

// 允许直接运行：node test/offline_swarm_test.js
if (import.meta.url === `file://${process.argv[1].replace(/\\/g, '/')}` ||
    process.argv[1] && process.argv[1].endsWith('offline_swarm_test.js')) {
  run().then(() => { console.log('offline_swarm_test OK'); process.exit(0); })
    .catch((e) => { console.error('offline_swarm_test FAILED:', e.message); process.exit(1); });
}
