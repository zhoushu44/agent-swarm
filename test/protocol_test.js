// 协议分支测试 —— 覆盖离线闭环之外的关键路径：
//   1. 门禁失败 → item 退回 pending（不是 done）+ 发布 FAIL 上下文
//   2. 门禁成功 → item done
//   3. autoscaler 按 pending 扩容（命名不撞）+ retire 标记不被 worker 循环覆盖
//   4. stop() 立即中止正在跑的长 turn（abort 通道）
//   5. 长 turn 中租约自动续期（submit 不被 409 拒）
//   6. worker 每回合刷新基线（不陈旧 → 无大面积冲突）
//   7. 集成在独立 worktree 里做，绝不碰用户主工作区
//   8. 单一大任务靠拆解深度信号扩容（不只依赖 pending 广度）
//   9. reaper 回收退场 worker 的 worktree（防长跑磁盘泄漏），但不误删待集成的分支
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Repo, git } from '../src/gitutil.js';
import { Store } from '../src/store.js';
import { createServer } from '../src/server.js';
import { Runner } from '../src/runner.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const results = [];
function check(name, cond, extra = '') {
  results.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${extra ? '  — ' + extra : ''}`);
  return !!cond;
}
async function waitFor(fn, { timeoutMs = 30000, intervalMs = 250, label = 'condition' } = {}) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (await fn()) return true;
    await sleep(intervalMs);
  }
  throw new Error(`timeout waiting for ${label}`);
}
function activeCount(runner) {
  return [...runner.workers.values()].filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
}
function tmpRepo() { return fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-proto-')); }

// ---------- 场景 1：门禁失败 → item 退回 pending ----------
async function scenarioGateFail() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'bad change', createdBy: 'test' });
  const PORT = 17898;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'gate fail test',
    // 门禁：固定失败（避免引号嵌套导致 cmd 挂起）
    testCmd: `node -e process.exit(1)`,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/bad.txt oops',
      'commit bad change',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say submitted',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.integrations().length >= 1 && store.item('t1').status === 'pending',
    { label: 'gate fail resolved', timeoutMs: 30000 });

  check('门禁失败：item 退回 pending（未 done）', store.item('t1').status === 'pending', store.item('t1').status);
  const ints = store.integrations();
  check('门禁失败：集成记录 merged=0', ints.some((i) => i.merged === 0),
    JSON.stringify(ints.map((i) => ({ merged: i.merged, passed: i.test_passed }))));
  check('门禁失败：发布 FAIL 上下文', store.searchContext('integration failed', 20).length > 0);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 2：门禁成功 → item done ----------
async function scenarioGatePass() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'good change', createdBy: 'test' });
  const PORT = 17897;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'gate pass test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/good.txt ok',
      'commit good change',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say submitted',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'done', { label: 'item done', timeoutMs: 30000 });

  check('门禁成功：item done', store.item('t1').status === 'done');
  const ints = store.integrations();
  check('门禁成功：集成记录 merged=1', ints.some((i) => i.merged === 1));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 3：autoscaler 扩容 + retire 生效 ----------
async function scenarioAutoscale() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  for (let i = 1; i <= 4; i++) store.addItem({ id: `t${i}`, title: `task ${i}`, createdBy: 'test' });
  const PORT = 17896;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'autoscale test', testCmd: null,
    maxTurns: 99, idleTurnsToExit: 99, minWorkers: 1, maxWorkers: 4,
  });
  runner.echoScripts = { '*': 'say IDLE' }; // 不真正干活，只让 worker 进入 idle
  runner.startAutoscaler();
  await runner.spawnWorker('w1'); // 模拟 cli 初始拉起

  await waitFor(() => activeCount(runner) >= 4, { label: 'autoscale up to 4', timeoutMs: 30000 });
  const names = [...runner.workers.keys()];
  check('autoscaler 扩容到 4', activeCount(runner) >= 4, `active=${activeCount(runner)}`);
  check('worker 命名不撞名', names.length === new Set(names).size, names.join(','));

  // retire 标记不被 worker 循环覆盖
  const w = [...runner.workers.values()][0];
  await waitFor(() => w.status === 'idle', { label: 'worker idle', timeoutMs: 20000 });
  w.status = 'retired';
  await sleep(3000);
  check('retire 标记不被覆盖（worker 退出）', w.status === 'retired', w.status);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 4：stop() 立即中止正在跑的长 turn（bug 2） ----------
// 用真的 sleep 子进程模拟"卡住的 harness"，验证 abort 后 drain 秒级返回而非等超时。
async function scenarioAbort() {
  const { run } = await import('../src/harness.js');
  const ac = new AbortController();
  const t0 = Date.now();
  const p = run([process.execPath, '-e', 'setTimeout(()=>{}, 60000)'], { cwd: process.cwd(), timeoutMs: 60000, signal: ac.signal });
  await sleep(500); // 确保子进程已起
  ac.abort();
  const r = await p;
  const elapsed = Date.now() - t0;
  check('abort 后子进程被立即杀掉', r.aborted === true, `elapsed=${elapsed}ms`);
  check('abort 在 5s 内返回（未等到 60s 超时）', elapsed < 5000, `elapsed=${elapsed}ms`);
}

// ---------- 场景 5：租约自动续期（bug 1） ----------
// 租约 3s，但 turn 持续 6s（模拟长 turn），续期循环应保证 submit 不被 409 拒。
async function scenarioLeaseRenew() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'long task', createdBy: 'test' });
  const PORT = 17894;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'lease renew test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
    leaseSec: 3, // 短租约，逼出续期需求
  });
  // 脚本：claim 后睡 6s（> 2 个租约周期），再提交。无续期则租约早已过期 → submit 409
  runner.echoScripts = {
    w1: [
      'claim t1',
      'sleep 6000',
      'file src/x.txt ok',
      'commit long task',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startLeaseRenewer(); // 关键：续期循环
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'done', { label: 'long turn submit accepted', timeoutMs: 30000 });

  check('长 turn 中租约被自动续期，submit 成功', store.item('t1').status === 'done');
  const subs = store.db.prepare('SELECT state FROM submissions').all();
  check('submission 已 merged（未被 409 拒）', subs.some((s) => s.state === 'merged'), JSON.stringify(subs));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 6：worker 基线刷新（bug 3） ----------
// w1 先把一个文件合并进集成分支；随后 w2 刷新基线应能拿到该文件（旧实现拿不到）。
async function scenarioBaselineSync() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'base change', createdBy: 'test' });
  store.addItem({ id: 't2', title: 'dependent change', createdBy: 'test' });
  const PORT = 17893;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'baseline sync test', testCmd: null,
    maxTurns: 2, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/base.txt from-w1',
      'commit base',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'done', { label: 'w1 done', timeoutMs: 30000 });

  // 集成分支现在应有 src/base.txt
  const onInt = git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir);
  check('集成分支含 w1 的 src/base.txt', onInt.includes('src/base.txt'));

  // 新起 w2（出生基线就是最新），再手动 syncWorktree 应幂等无冲突
  await runner.spawnWorker('w2');
  const sync = await runner.repo.syncWorktree('w2');
  check('syncWorktree 无冲突返回 ok', sync.ok === true, JSON.stringify(sync));
  const w2Dir = path.join(repoDir, '.swarm', 'worktrees', 'w2');
  check('w2 worktree 拿到集成分支的最新文件', fs.existsSync(path.join(w2Dir, 'src', 'base.txt')));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 7：集成不污染用户主工作区（bug 4） ----------
// 用户在 main 上有未提交改动；集成器合并后，用户的改动必须原封不动、且当前分支仍是 main。
async function scenarioIntegrationIsolation() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  // 用户在主工作区制造未提交改动（模拟正在写码的人）
  fs.writeFileSync(path.join(repoDir, 'user_wip.txt'), 'user work in progress\n');
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'isolated change', createdBy: 'test' });
  const PORT = 17892;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'integration isolation test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/feat.txt merged-by-worker',
      'commit feat',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'done', { label: 'integration done', timeoutMs: 30000 });

  // 主工作区：仍在 main、用户的未提交文件还在、且没有被 worker 的文件污染
  const curBranch = git(['rev-parse', '--abbrev-ref', 'HEAD'], repoDir);
  check('集成后主工作区仍停在 main', curBranch === 'main', curBranch);
  check('用户未提交改动仍在', fs.existsSync(path.join(repoDir, 'user_wip.txt')));
  const dirty = git(['status', '--porcelain'], repoDir);
  check('用户的改动仍是未提交状态（未被提交/覆盖）', dirty.includes('user_wip.txt'), dirty);
  check('worker 的文件未出现在主工作区', !fs.existsSync(path.join(repoDir, 'src', 'feat.txt')));
  // 集成 worktree 里才有合并结果
  check('集成 worktree 存在', fs.existsSync(path.join(repoDir, '.swarm', 'integration', '.git')));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 8：单一大任务靠拆解深度信号扩容（bug 5） ----------
// 只有一个大 item（pending 恒为 1），worker 不断把它拆成子项。
// 需求："面板都是大任务时也要能上很多 agent"——拆解是扩大并行度的唯一机制。
async function scenarioDeepAutoscale() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 'big', title: 'one huge task', createdBy: 'test' });
  const PORT = 17891;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'deep autoscale test', testCmd: null,
    maxTurns: 99, idleTurnsToExit: 99, minWorkers: 2, maxWorkers: 8,
  });
  // 每个 worker 每回合往 big 下挂 3 个子项（模拟"大任务自动拆解"），然后 IDLE。
  // 拆解让 open / recentSplit 升高 → 深度信号触发扩容。
  // title 必须带 {{peer}}/{{turn}} 保持唯一：addItem 对"同一父项下同名子项"做了幂等去重
  // （防止 N 个 worker 各自重拆同一个大 item），同名会被折叠成 1 项。
  runner.echoScripts = {
    '*': [
      'api POST /item {"peer":"{{peer}}","title":"sub of big {{peer}} {{turn}} a","parentId":"big"}',
      'api POST /item {"peer":"{{peer}}","title":"sub of big {{peer}} {{turn}} b","parentId":"big"}',
      'api POST /item {"peer":"{{peer}}","title":"sub of big {{peer}} {{turn}} c","parentId":"big"}',
      'say IDLE',
    ].join('\n'),
  };
  runner.startAutoscaler();
  await runner.spawnWorker('w1');

  await waitFor(() => activeCount(runner) >= 8, { label: 'deep autoscale to 8', timeoutMs: 60000 });
  check('单一大任务下深度信号扩容到 maxWorkers', activeCount(runner) >= 8, `active=${activeCount(runner)}`);
  check('确实产生了拆解子项', store.recentSplitCount() >= 1, `splits=${store.recentSplitCount()}`);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 9：minWorkers 是地板而非加法基线（bug 5 的公式缺陷） ----------
// minWorkers=4 但只有 1 个待办项时，旧的 `minWorkers + pending` 会算出 5（多养 1 个空转 worker）；
// 新公式 max(minWorkers, demand) 应恰好保持 4。
async function scenarioMinWorkersFloor() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 'only', title: 'single item', createdBy: 'test' });
  const PORT = 17890;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'minWorkers floor test', testCmd: null,
    maxTurns: 99, idleTurnsToExit: 99, minWorkers: 4, maxWorkers: 10,
  });
  runner.echoScripts = { '*': 'say IDLE' };
  runner.startAutoscaler();
  await runner.spawnWorker('w1');
  await waitFor(() => activeCount(runner) >= 4, { label: 'reach floor 4', timeoutMs: 30000 });
  await sleep(12000); // 让 autoscaler 多跑几轮，确认不会溢出到 5
  check('minWorkers 作为地板：恰好 4 个 worker（非 5）', activeCount(runner) === 4, `active=${activeCount(runner)}`);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 9b：maxWorkers=0 无静态上限，纯靠任务量驱动扩容 ----------
// 20 个待办项时，旧 maxWorkers 默认（如 8）会卡住并行度；0 表示不设上限，
// want 只由 demand（open 任务数）决定。资源护栏在本测试关闭（离线）。
async function scenarioUnlimitedWorkers() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  for (let i = 1; i <= 20; i++) store.addItem({ id: `u${i}`, title: `task ${i}`, createdBy: 'test' });
  const PORT = 17896;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'unlimited workers test', testCmd: null,
    maxTurns: 99, idleTurnsToExit: 99, minWorkers: 1, maxWorkers: 0,
  });
  runner.echoScripts = { '*': 'say IDLE' };
  runner.startAutoscaler();
  await runner.spawnWorker('w1');
  // open=20 → demand=20 → capped=min(20, max(8,40))=20 → want=20（无上限钳制）
  await waitFor(() => activeCount(runner) >= 12, { label: 'unbounded autoscale past old default', timeoutMs: 60000 });
  check('maxWorkers=0：扩容突破静态上限（active >= 12）', activeCount(runner) >= 12, `active=${activeCount(runner)}`);
  check('maxWorkers=0：status 报告 maxWorkers=null（动态）', runner.status().maxWorkers === null);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}
// EXP4 实测：10 个 worker 把同一个 big 重拆成 91/64 个重复子项（本应 10 个）。
// 根因是 (parent, title) 精确去重挡不住 LLM 的标题变体。本场景直接打 HTTP，
// 用同一交付物的多种写法建子项，断言只落 1 个；不同交付物仍各自独立。
async function scenarioSplitDedup() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 'big', title: 'one huge task', createdBy: 'test' });
  const PORT = 17887;
  const { server } = await createServer(store, { port: PORT });
  const base = `http://127.0.0.1:${PORT}`;
  const post = (path, body) => fetch(base + path, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  }).then((r) => r.json());

  // 同一交付物 src/m1.js 的 4 种写法（LLM 真实会产出的变体）
  const variants = [
    'src/m1.js',
    '创建 src/m1.js',
    '创建 src/m1.js (f_1(x)=x+1)',
    '创建 src/m1.js（导出 f_1(x)=x+1）',
  ];
  const ids = [];
  for (const title of variants) {
    const r = await post('/item', { peer: 'w1', title, parentId: 'big' });
    ids.push(r.item?.id);
  }
  check('同一交付物的标题变体折叠为 1 个子项', new Set(ids).size === 1, `ids=${JSON.stringify(ids)}`);
  check('big 下确实只有 1 个子项', store.stmts.childCount.get('big').n === 1, `n=${store.stmts.childCount.get('big').n}`);
  const dedupEvents = store.eventsAfter(0, 500).filter((e) => e.type === 'item.deduped');
  check('重复拆解被记录为 item.deduped 事件', dedupEvents.length === 3, `n=${dedupEvents.length}`);
  check('去重键是抽取出的文件路径', dedupEvents[0]?.payload?.key === 'f:src/m1.js', `key=${dedupEvents[0]?.payload?.key}`);

  // 不同交付物必须各自独立（不能过度折叠）
  const r2 = await post('/item', { peer: 'w1', title: '创建 src/m2.js', parentId: 'big' });
  check('不同交付物不会被误合并', r2.item?.id !== ids[0] && store.stmts.childCount.get('big').n === 2, `n=${store.stmts.childCount.get('big').n}`);

  // 无文件名的中文项走 t: 回退分支：去空白/标点后折叠
  const r3 = await post('/item', { peer: 'w1', title: '实现登录', parentId: 'big' });
  const r4 = await post('/item', { peer: 'w2', title: '实现 登录', parentId: 'big' });
  check('无文件名的中文项按归一化文本折叠', r3.item?.id === r4.item?.id);

  // 跨父项折叠：同一文件交付物挂在两个不同父项下（两个 worker 各自拆解不同的大任务），
  // 必须折叠成同一个 item——否则两个 worker 会同时改同一个文件（用户观察到的"改到一起去了"）。
  store.addItem({ id: 'big2', title: 'another huge task', createdBy: 'test' });
  const cross = await post('/item', { peer: 'w2', title: '创建 src/m1.js', parentId: 'big2' });
  check('跨父项的同文件交付物折叠到同一个 item', cross.item?.id === ids[0], `id=${cross.item?.id} want=${ids[0]}`);
  check('big2 下不会新建重复子项', store.stmts.childCount.get('big2').n === 0, `n=${store.stmts.childCount.get('big2').n}`);
  const globalDedup = store.eventsAfter(0, 500).filter((e) => e.type === 'item.deduped' && e.payload?.scope === 'global');
  check('跨父项折叠记录 scope=global 的 item.deduped 事件', globalDedup.length === 1, `n=${globalDedup.length}`);

  // t: 键不跨父项折叠（无文件名的语义文本跨父项合并会误合并不同任务）
  const t1 = await post('/item', { peer: 'w1', title: '实现登录', parentId: 'big2' });
  check('无文件名的项不跨父项折叠', t1.item?.id !== r3.item?.id, `id=${t1.item?.id}`);

  // 语义折叠（同父项、无文件名）：标题不同但任务差不多的项必须折成 1 个。
  // 这是 EXP4 之外的另一类重复来源——没有文件路径当锚点，归一化文本也不相等。
  store.addItem({ id: 'big3', title: 'semantic parent', createdBy: 'test' });
  const s1 = await post('/item', { peer: 'w1', title: '实现登录', parentId: 'big3' });
  const s2 = await post('/item', { peer: 'w2', title: '实现用户登录功能', parentId: 'big3' });
  check('语义近似项（无文件名）折叠为 1 个', s1.item?.id === s2.item?.id, `a=${s1.item?.id} b=${s2.item?.id}`);
  check('折叠后 big3 下只有 1 个子项', store.stmts.childCount.get('big3').n === 1, `n=${store.stmts.childCount.get('big3').n}`);

  // 保守性：泛化短标题不能被吸走，语义不同的任务不能互相吞并，长度悬殊的不能合并
  const n1 = await post('/item', { peer: 'w1', title: '实现注册', parentId: 'big3' });
  check('语义不同的任务不会被误合并', n1.item?.id !== s1.item?.id, `id=${n1.item?.id}`);
  const n2 = await post('/item', { peer: 'w1', title: '测试', parentId: 'big3' });
  const n3 = await post('/item', { peer: 'w2', title: '添加单元测试', parentId: 'big3' });
  check('泛化短标题（token 过少）不被吸入', n2.item?.id !== n3.item?.id, `a=${n2.item?.id} b=${n3.item?.id}`);
  const n4 = await post('/item', { peer: 'w1', title: '实现登录并接入第三方 OAuth 单点登录与刷新令牌', parentId: 'big3' });
  check('长度悬殊的项不被吸入', n4.item?.id !== s1.item?.id, `id=${n4.item?.id}`);
  const semanticDedup = store.eventsAfter(0, 500).filter((e) => e.type === 'item.deduped' && e.payload?.scope === 'semantic');
  check('语义折叠记录 scope=semantic 的 item.deduped 事件', semanticDedup.length === 1, `n=${semanticDedup.length}`);

  server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 13：LLM 判官去重（两阶段：词法召回 → judge 终审） ----------
// 词面折叠挡不住"实现登录" vs "支持账号密码登录"这类同义变体；判官在场时
// 词面不再抢先合并（semanticFold=false），生死由 judge 定：
//   路径 A：judge 判 duplicate → 折叠到既有项，记 scope=judge 事件
//   路径 B：judge 判 related/distinct → 不合并（宁漏勿错：误合并 = 静默丢活）
//   路径 C：judge 失败/inconclusive → 回退词面折叠（scope=semantic-fallback）
// mock judge 不打真实 API，只验证协议接线与回退逻辑。
async function scenarioJudgeDedup() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 'big', title: 'judge parent', createdBy: 'test' });
  // 同义变体对：词面 key 不同、token 也不互为子集——只有 judge 能判重。
  // "实现登录" 与 "支持账号密码登录"：token 集 {实现,登,录} vs {支持,账,号,密,码,登,录}，
  // 非子集关系，词面折叠不命中，正好验证 judge 路径。
  store.addItem({ id: 'login-a', title: '实现登录', detail: '用户名+密码登录接口', parentId: 'big', createdBy: 'test' });

  // 路径 A：judge 判 duplicate
  const judgeA = { judgeIsDuplicate: async (n, cands) => {
    check('judge 收到新项与候选（含 detail）', n.title === '支持账号密码登录' && cands.length === 1 && cands[0].id === 'login-a', JSON.stringify(cands));
    return { duplicate: true, matchId: cands[0].id, reason: 'same deliverable', inconclusive: false };
  } };
  // 三阶段各用独立端口：server.close() 是异步的，同端口立刻重绑在 Windows 上可能 EADDRINUSE
  let { server } = await createServer(store, { port: 17890, dedupJudge: judgeA });
  let base = 'http://127.0.0.1:17890';
  let post = (p, body) => fetch(base + p, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  }).then((r) => r.json());

  const a1 = await post('/item', { peer: 'w1', title: '支持账号密码登录', detail: '用户名+密码登录接口', parentId: 'big' });
  check('路径A：judge 判 duplicate → 折叠到既有项', a1.item?.id === 'login-a', `id=${a1.item?.id}`);
  check('路径A：big 下没有新建子项', store.stmts.childCount.get('big').n === 1, `n=${store.stmts.childCount.get('big').n}`);
  const judgeEvents = store.eventsAfter(0, 500).filter((e) => e.type === 'item.deduped' && e.payload?.scope === 'judge');
  check('路径A：记录 scope=judge 的 item.deduped 事件', judgeEvents.length === 1 && judgeEvents[0]?.payload?.id === 'login-a', `n=${judgeEvents.length}`);
  server.close();

  // 路径 B：judge 判 distinct（同义词面相似但任务不同）→ 必须保持独立
  store.addItem({ id: 'reg-a', title: '实现注册', detail: '新用户注册流程', parentId: 'big', createdBy: 'test' });
  const judgeB = { judgeIsDuplicate: async () => ({ duplicate: false, matchId: null, reason: 'distinct work', inconclusive: false }) };
  ({ server } = await createServer(store, { port: 17891, dedupJudge: judgeB }));
  base = 'http://127.0.0.1:17891';
  post = (p, body) => fetch(base + p, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  }).then((r) => r.json());
  const b1 = await post('/item', { peer: 'w1', title: '实现用户注册功能', detail: '新用户注册流程', parentId: 'big' });
  check('路径B：judge 判非 duplicate → 保持独立', b1.item?.id !== 'reg-a' && !!b1.item?.id, `id=${b1.item?.id}`);
  check('路径B：新项已落库', store.stmts.childCount.get('big').n === 3, `n=${store.stmts.childCount.get('big').n}`);
  server.close();

  // 路径 C：judge inconclusive（网络/限流失败）→ 回退词面折叠
  // 用 token 子集对（词面能折的）验证回退：judge 失败时 foldSemantic 接住。
  const judgeC = { judgeIsDuplicate: async () => ({ duplicate: false, matchId: null, reason: 'api down', inconclusive: true }) };
  ({ server } = await createServer(store, { port: 17892, dedupJudge: judgeC }));
  base = 'http://127.0.0.1:17892';
  post = (p, body) => fetch(base + p, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  }).then((r) => r.json());
  const c1 = await post('/item', { peer: 'w1', title: '增加邮箱验证码登录方式', parentId: 'big' });
  // "增加邮箱验证码登录方式" 与既有项（实现登录/实现注册/实现用户注册功能）token 互不为子集
  // → 词面回退也不命中 → 应新建而非折叠（回退路径下也不误合并）
  check('路径C：judge 失败且词面不命中 → 新建', !!c1.item?.id && c1.item?.id !== 'reg-a' && c1.item?.id !== b1.item?.id, `id=${c1.item?.id}`);
  // 再用一个真子集对验证 foldSemantic 回退真的会折叠
  const c2 = await post('/item', { peer: 'w1', title: '实现用户注册', parentId: 'big' });
  const fbEvents = store.eventsAfter(0, 500).filter((e) => e.type === 'item.deduped' && e.payload?.scope === 'semantic-fallback');
  check('路径C：词面子集对经 foldSemantic 回退折叠', fbEvents.length >= 1, `n=${fbEvents.length}, foldedTo=${fbEvents[0]?.payload?.id}`);

  server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}


// ---------- 场景 10：reaper 回收退场 worker 的 worktree（bug 8） ----------
// 三个要点：正常退场的 worktree 被回收；待集成的分支不被误删；worker 从池里移除。
async function scenarioReaper() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'work', createdBy: 'test' });
  const PORT = 17889;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'reaper test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.reaperIntervalMs = 1000; // 测试加速
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/a.txt hi',
      'commit work',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startIntegrator();
  runner.startReaper();
  await runner.spawnWorker('w1');
  const wtDir = path.join(repoDir, '.swarm', 'worktrees', 'w1');
  check('worker 启动时 worktree 存在', fs.existsSync(wtDir));

  // 等集成完成（此时 w1 已退场，且无待集成提交 → 可回收）
  await waitFor(() => store.item('t1').status === 'done', { label: 'w1 work integrated', timeoutMs: 30000 });
  await waitFor(() => !runner.workers.has('w1'), { label: 'w1 reaped from pool', timeoutMs: 15000 });
  check('退场 worker 已从池中移除', !runner.workers.has('w1'));
  check('退场 worker 的 worktree 已被回收', !fs.existsSync(wtDir), `exists=${fs.existsSync(wtDir)}`);
  // 集成分支仍在（回收的是 worker worktree，不是集成结果）
  const onInt = git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir);
  check('回收后集成分支结果不受影响', onInt.includes('src/a.txt'));

  // 分支也已删除
  const branches = git(['branch', '--list', 'swarm/w-w1'], repoDir);
  check('退场 worker 的分支已删除', branches.trim() === '', branches);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 11：reaper 不误删"待集成"的 worktree（bug 8 的安全护栏） ----------
// worker 已退场、循环已结束，但提交还 queued（集成器没跑）。此时回收会删掉分支，
// 让集成器 merge 找不到分支 → 集成永久失败。reaper 必须等提交 resolve 后再回收。
async function scenarioReaperGuard() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'pending integration', createdBy: 'test' });
  const PORT = 17888;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'reaper guard test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.reaperIntervalMs = 500;
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/g.txt keep-me',
      'commit guard',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startReaper(); // 故意不启动集成器：提交会一直 queued
  await runner.spawnWorker('w1');
  const wtDir = path.join(repoDir, '.swarm', 'worktrees', 'w1');

  // 等 worker 退场 + 提交入队
  await waitFor(() => runner.workers.get('w1')?.loopDone === true, { label: 'w1 loop done', timeoutMs: 20000 });
  await waitFor(() => store.hasQueuedSubmissionForPeer('w1'), { label: 'submission queued', timeoutMs: 10000 });
  await sleep(2000); // 给 reaper 至少 4 个周期，确认它没动手

  check('有待集成提交时，reaper 不回收 worktree', fs.existsSync(wtDir), `exists=${fs.existsSync(wtDir)}`);
  check('有待集成提交时，worker 仍在池中', runner.workers.has('w1'));
  check('分支仍在（集成器还能 merge）', git(['branch', '--list', 'swarm/w-w1'], repoDir).trim() !== '');

  // 现在启动集成器：提交被 resolve 后，reaper 应能回收
  runner.startIntegrator();
  await waitFor(() => store.item('t1').status === 'done', { label: 'integrated after guard', timeoutMs: 30000 });
  await waitFor(() => !runner.workers.has('w1'), { label: 'reaped after resolve', timeoutMs: 15000 });
  check('提交 resolve 后 reaper 正常回收', !fs.existsSync(wtDir));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 12：m3 结构三连检门禁前置（语法烂提交在 testCmd 之前被拦截） ----------
// testCmd 是会通过的（exit 0）——若结构检查不存在/失效，烂 JS 会直接落地。
// 断言它必须被 stage='structure' 拦下：item 退回 pending、FAIL 上下文带 baseline、集成记录 test_passed=0。
async function scenarioStructureGate() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'broken js', createdBy: 'test' });
  const PORT = 17879;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'structure gate test',
    testCmd: 'node -e process.exit(0)', // 门禁本身会过 → 只有结构检查能拦下这个提交
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/broken.js function broken( {', // 单行非法 JS：node --check 必挂
      'commit broken',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say submitted',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.integrations().length >= 1 && store.item('t1').status === 'pending',
    { label: 'structure gate rejected', timeoutMs: 30000 });

  const failCtx = store.searchContext('stage=structure', 10);
  check('结构门禁：item 退回 pending（testCmd 未被烧）', store.item('t1').status === 'pending');
  check('结构门禁：发布 stage=structure 的 FAIL 上下文', failCtx.length > 0,
    JSON.stringify(failCtx.map((c) => c.text.slice(0, 120))));
  check('结构门禁：FAIL 文本含 syntax error 与文件名',
    failCtx.length > 0 && /syntax error in src\/broken\.js/.test(failCtx[0].text),
    failCtx[0]?.text.slice(0, 200));
  check('结构门禁：FAIL 文本带 baseline 回滚点（m1\'）',
    failCtx.length > 0 && /baseline=[0-9a-f]{6,}/.test(failCtx[0].text) && !/baseline=unknown/.test(failCtx[0].text),
    failCtx[0]?.text.match(/baseline=\S+/)?.[0]);
  const ints = store.integrations();
  check('结构门禁：集成记录 merged=0 且 test_passed=0', ints.some((i) => i.merged === 0 && i.test_passed === 0),
    JSON.stringify(ints.map((i) => ({ merged: i.merged, passed: i.test_passed }))));
  // 集成分支确实没被污染
  const onInt = git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir);
  check('结构门禁：烂文件未落地集成分支', !onInt.includes('src/broken.js'));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });

  // ---- 单元级补充：conflict marker 成对检测（echo file 只写单行，E2E 写不出成对标记）----
  const repoDir2 = tmpRepo();
  const repo2 = new Repo(repoDir2); repo2.init();
  const itDir = repo2.ensureIntegrationWorktree();
  // _structureCheck 以 this.integrationHead 为 diff base：先记录基线再提交
  repo2.integrationHead = git(['rev-parse', 'HEAD'], repoDir2);
  fs.writeFileSync(path.join(itDir, 'doc.md'), '<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n');
  git(['add', 'doc.md'], itDir);
  git(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', 'markers'], itDir);
  let sc = await repo2._structureCheck(itDir);
  check('结构单元：成对 conflict marker 被拦截', sc.ok === false && /conflict marker/.test(sc.reason),
    JSON.stringify(sc));
  // 反例：仅 `=======`（markdown setext 标题下划线）不得误报
  const repoDir3 = tmpRepo();
  const repo3 = new Repo(repoDir3); repo3.init();
  const itDir3 = repo3.ensureIntegrationWorktree();
  repo3.integrationHead = git(['rev-parse', 'HEAD'], repoDir3);
  fs.writeFileSync(path.join(itDir3, 'readme.md'), 'Title\n=======\n\nbody\n');
  git(['add', 'readme.md'], itDir3);
  git(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', 'setext'], itDir3);
  sc = await repo3._structureCheck(itDir3);
  check('结构单元：setext 标题下划线不误报', sc.ok === true, JSON.stringify(sc));

  fs.rmSync(repoDir2, { recursive: true, force: true });
  fs.rmSync(repoDir3, { recursive: true, force: true });
}

// ---------- 场景 13：P5 冲突吸收 + redo 预算退场 ----------
// 两个 worker 同时 spawn、同写 src/shared.txt 不同内容：集成器合完赢家后，
// 输家的 sync（真实 merge）必然与集成分支冲突 → skew 累积。
// P5 语义：skew≥3 不再挂起，而是 备份分叉线(swarm/backup-<name>) + reset 回集成分支 +
// 广播 CONFLICT 上下文（HOT FILES）→ skew 归零重做；redo 预算耗尽 → retired_redo_exhausted。
// 关键：必须同时 spawn——若输家后出生，其基线已含赢家文件且集成分支不再推进（纪元短路），永不冲突。
async function scenarioSyncSkew() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'collide a', createdBy: 'test' });
  store.addItem({ id: 't2', title: 'collide b', createdBy: 'test' });
  const PORT = 17878;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'sync skew test', testCmd: null,
    maxTurns: 40, idleTurnsToExit: 99, minWorkers: 2, maxWorkers: 2,
  });
  // 各 claim 各的 item（echo 的 claim 不支持 shell 条件语法），但同写一个文件：
  // 两分支同 base 各改 src/shared.txt → 第二个被集成的必然 stage='merge' 失败，
  // 其后续基线刷新（真实 merge）也必然冲突 → skew 累积。
  // 吸收（reset 回集成分支）后输家基线已含赢家内容 → 重做写 from-<loser> 干净合并。
  const collide = (item, own) => [
    `claim ${item}`,
    `file src/shared.txt ${own}`,
    'commit collide',
    'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
    'say submitted',
  ].join('\n');
  runner.echoScripts = { w1: collide('t1', 'from-w1'), w2: collide('t2', 'from-w2') };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await runner.spawnWorker('w2');

  // 输家 = 第二个被集成的提交的 peer：其基线刷新（真实 merge）与集成分支冲突
  await waitFor(() => store.eventsAfter(0, 500).some((e) => e.type === 'worker.sync_conflict'),
    { label: 'first sync conflict', timeoutMs: 30000 });
  const conflictEv = store.eventsAfter(0, 500).find((e) => e.type === 'worker.sync_conflict');
  check('syncSkew：冲突事件带文件清单', (conflictEv.payload.files || []).includes('src/shared.txt'),
    JSON.stringify(conflictEv.payload));

  // P5 吸收：skew 达 3 → conflict_absorbed 事件（redo#、备份分支名）
  const loserName = conflictEv.payload.name;
  await waitFor(() => {
    return store.eventsAfter(0, 500).some((e) => e.type === 'worker.conflict_absorbed' && e.payload.name === loserName);
  }, { label: 'loser absorbed', timeoutMs: 60000 });
  const absEv = store.eventsAfter(0, 500).find((e) => e.type === 'worker.conflict_absorbed' && e.payload.name === loserName);
  check('P5 吸收：事件带 redo 序号与备份分支', absEv.payload.redo === 1 && absEv.payload.backup === `swarm/backup-${loserName}`,
    JSON.stringify(absEv.payload));
  check('P5 吸收：备份分支真实存在', git(['branch', '--list', `swarm/backup-${loserName}`], repoDir).trim() !== '',
    git(['branch', '--list', `swarm/backup-${loserName}`], repoDir));
  // CONFLICT 上下文已广播（HOT FILES 清单进共享 context，全队下回合简报可见）
  const confCtx = store.db.prepare("SELECT * FROM context WHERE kind='CONFLICT' AND peer=?").all(loserName);
  check('P5 吸收：CONFLICT 上下文含 HOT FILES', confCtx.length > 0 && (confCtx[0].text || '').includes('src/shared.txt'),
    JSON.stringify((confCtx[0] || {}).text || '').slice(0, 160));
  // 吸收 ≠ 退场：loser 重置基线继续干，t2 应最终完成（重做后干净合并）
  await waitFor(() => store.item('t2').status === 'done', { label: 'loser item done after absorb', timeoutMs: 60000 });
  check('P5 吸收：吸收后输家完成自己的 item', store.item('t2').status === 'done',
    JSON.stringify({ t1: store.item('t1').status, t2: store.item('t2').status }));
  // 吸收 ≠ 因冲突退场：t2 done 之后 board 空了，maxTurns/idle 软上限合法退休不算输。
  // 真正的失败信号是 retired_skew / retired_redo_exhausted / retired_reset_failed ——
  // 那三种说明 P5 吸收机制没救回这个 worker。
  const loserRetireEvs = store.eventsAfter(0, 2000)
    .filter((e) => e.type.startsWith('worker.retired') && e.payload.name === loserName)
    .map((e) => e.type);
  const badRetire = loserRetireEvs.find((t) => /_skew|_redo_exhausted|_reset_failed/.test(t));
  check('P5 吸收：输家未因冲突退场', !badRetire, JSON.stringify(loserRetireEvs));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 13b：P5 redo 预算耗尽 → retired_redo_exhausted ----------
// maxConflictRedo=0（实例属性，runner 构造器不收）：skew 达 3 即预算耗尽退场。
async function scenarioRedoExhausted() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'collide a', createdBy: 'test' });
  store.addItem({ id: 't2', title: 'collide b', createdBy: 'test' });
  const PORT = 17876;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'redo budget test', testCmd: null,
    maxTurns: 40, idleTurnsToExit: 99, minWorkers: 2, maxWorkers: 2,
  });
  runner.maxConflictRedo = 0; // 零预算：skew 一到 3 就退场，不走吸收
  const collide = (item, own) => [
    `claim ${item}`,
    `file src/shared.txt ${own}`,
    'commit collide',
    'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
    'say submitted',
  ].join('\n');
  runner.echoScripts = { w1: collide('t1', 'from-w1'), w2: collide('t2', 'from-w2') };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await runner.spawnWorker('w2');

  await waitFor(() => {
    return store.eventsAfter(0, 500).some((e) => e.type === 'worker.retired_redo_exhausted');
  }, { label: 'retired by redo exhaustion', timeoutMs: 60000 });
  const retEv = store.eventsAfter(0, 500).find((e) => e.type === 'worker.retired_redo_exhausted');
  const retName = retEv.payload.name;
  check('P5 预算耗尽：redo=0 时 skew≥3 直接退场', retEv.payload.skew >= 3 && retEv.payload.redos === 0,
    JSON.stringify(retEv.payload));
  check('P5 预算耗尽：退场时 status=retired', runner.workers.get(retName)?.status === 'retired',
    String(runner.workers.get(retName)?.status));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 15：P1 批量集成——多条不相交提交一次门禁成批 ----------
// 构造法：先 spawn 两个 worker 让它们各自提交入队（改的文件不相交），再 startIntegrator——
// 集成器首轮 poll 就看到 2 条 → 走 _groupDisjoint → _integrateBatch（不产 integration.start）。
async function scenarioBatchIntegrate() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'batch a', createdBy: 'test' });
  store.addItem({ id: 't2', title: 'batch b', createdBy: 'test' });
  const PORT = 17875;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'batch integrate test', testCmd: null,
    maxTurns: 8, idleTurnsToExit: 99, minWorkers: 2, maxWorkers: 2,
  });
  const mk = (item, file) => [
    `claim ${item}`,
    `file ${file} content-of-${item}`,
    'commit batch change',
    'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
    'say submitted',
  ].join('\n');
  runner.echoScripts = { w1: mk('t1', 'src/batch-a.txt'), w2: mk('t2', 'src/batch-b.txt') };
  // 关键时序：不先启动集成器，等双提交都入队后再启动 → 首轮必走批量路径
  await runner.spawnWorker('w1');
  await runner.spawnWorker('w2');
  await waitFor(() => store.hasQueuedSubmissionForPeer('w1') && store.hasQueuedSubmissionForPeer('w2'),
    { label: 'both submissions queued', timeoutMs: 60000 });
  runner.startIntegrator();
  await waitFor(() => store.item('t1').status === 'done' && store.item('t2').status === 'done',
    { label: 'batch merged', timeoutMs: 60000 });

  const evs = store.eventsAfter(0, 500);
  const bs = evs.find((e) => e.type === 'integration.batch_start');
  const bo = evs.find((e) => e.type === 'integration.batch_ok');
  check('P1 批量集成：batch_start 两条同批', !!bs && bs.payload.count === 2
    && bs.payload.peers.includes('w1') && bs.payload.peers.includes('w2'),
    bs ? JSON.stringify(bs.payload) : '(no batch_start)');
  check('P1 批量集成：batch_ok 覆盖两 item', !!bo && bo.payload.count === 2
    && bo.payload.itemIds.includes('t1') && bo.payload.itemIds.includes('t2'),
    bo ? JSON.stringify(bo.payload) : '(no batch_ok)');
  check('P1 批量集成：批量路径不产生 integration.start（单条路径专有）',
    !evs.some((e) => e.type === 'integration.start'));
  const ps = store.db.prepare("SELECT * FROM context WHERE kind='PATCH_SUMMARY'").all();
  check('P1 批量集成：一条 PATCH_SUMMARY 概括整批', ps.length === 1 && ps[0].text.includes('batch-merged 2 items'),
    ps.length ? ps[0].text.slice(0, 120) : '(none)');
  check('P1 批量集成：两 item 均 done',
    store.item('t1').status === 'done' && store.item('t2').status === 'done');
  const files = git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir);
  check('P1 批量集成：集成分支含两文件', files.includes('src/batch-a.txt') && files.includes('src/batch-b.txt'),
    files.replace(/\n/g, ','));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 16：P6 毒项熔断——门禁连拒 maxItemFails 次 → failed 终态 ----------
// maxItemFails=2 + 门禁恒挂：第 1 次拒绝 → retry_backoff（退回 pending 带退避窗口），
// 第 2 次拒绝 → item.failed 终态（不可再认领、不计 open/claimable）。
async function scenarioItemFuse() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'), { maxItemFails: 2 });
  store.addItem({ id: 't1', title: 'poison item', createdBy: 'test' });
  const PORT = 17874;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'item fuse test', testCmd: 'node -e process.exit(1)',
    maxTurns: 12, idleTurnsToExit: 99, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/bad.txt oops',
      'commit bad change',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say submitted',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'failed',
    { label: 'poison item fused', timeoutMs: 90000 });

  const evs = store.eventsAfter(0, 500);
  const bo = evs.find((e) => e.type === 'item.retry_backoff');
  const fe = evs.find((e) => e.type === 'item.failed' && e.payload.itemId === 't1');
  check('P6 毒项熔断：第 1 次拒绝先退避（fails=1）', !!bo && bo.payload.fails === 1,
    bo ? JSON.stringify(bo.payload) : '(no retry_backoff)');
  check('P6 毒项熔断：退避事件早于熔断事件', !!bo && !!fe && bo.id < fe.id);
  check('P6 毒项熔断：第 2 次拒绝熔断（reason 含 gate rejected 2x）',
    !!fe && /gate rejected 2x/.test(fe.payload.reason || ''), fe ? fe.payload.reason : '(no item.failed)');
  check('P6 毒项熔断：item 终态 failed', store.item('t1').status === 'failed');
  check('P6 毒项熔断：熔断项不再可认领/不占开放数',
    store.claimableCount() === 0 && store.openCount() === 0,
    `claimable=${store.claimableCount()} open=${store.openCount()}`);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 17：P7 预算熔断——token 超预算 → 剩余开放项全部 failed + 软停 ----------
// echo harness 不产出 usage，手工注入 tokenUsage 让 _tokensSpent() 越过闸门；
// 不注入 onBudgetExceeded 钩子 → 走 _stopped=true 软停分支。
async function scenarioBudgetFuse() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'burn tokens', createdBy: 'test' });
  store.addItem({ id: 't2', title: 'never touched', createdBy: 'test' });
  const PORT = 17873;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'budget fuse test', testCmd: null, budgetTokens: 1000,
    maxTurns: 20, idleTurnsToExit: 99, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    // 脚本必须秒结束：echo 是同步 Node 代码，脚本里的 sleep 会阻塞事件循环，
    // 连 /claim 的响应回调都得等它跑完——回合内长 sleep 等于把 worker 卡死在回合 1。
    w1: [
      'claim t1', // t1 → in_progress 并一直占着（不提交）
      'say holding',
    ].join('\n'),
  };
  await runner.spawnWorker('w1'); // 不开集成器：本场景与集成无关
  await waitFor(() => store.item('t1').status === 'in_progress', { label: 't1 claimed' });
  // 回合 1 结束（t1 占住）→ 注入超额用量 → 回合 2 开工前闸门触发
  await waitFor(() => store.eventsAfter(0, 200).some((e) => e.type === 'worker.turn_done'),
    { label: 'first turn done' });
  runner.tokenUsage.inputTokens = 99999; // 注入超额用量 → 下回合开工前闸门触发
  await waitFor(() => runner.budgetExceeded === true, { label: 'budget gate tripped', timeoutMs: 30000 });

  const evs = store.eventsAfter(0, 500);
  const be = evs.find((e) => e.type === 'budget.exceeded');
  const cb = evs.find((e) => e.type === 'budget.circuit_broken');
  check('P7 预算熔断：budget.exceeded 记录 spent/budget',
    !!be && be.payload.spent === 99999 && be.payload.budget === 1000,
    be ? JSON.stringify(be.payload) : '(no budget.exceeded)');
  check('P7 预算熔断：circuit_broken 熔断全部 2 个开放项',
    !!cb && cb.payload.failedItems === 2, cb ? JSON.stringify(cb.payload) : '(no circuit_broken)');
  check('P7 预算熔断：t1(in_progress)/t2(pending) 均 failed',
    store.item('t1').status === 'failed' && store.item('t2').status === 'failed');
  check('P7 预算熔断：无钩子时软停（_stopped=true）', runner._stopped === true);

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 18：P4 上下文治理——容量硬上限 + FACT 豁免 + FACT 兜底 ----------
// 直连 Store（构造器自动 mkdir）。maxContextRows 最小值 50。
// 阶段 A：60 条 NOTE → 收敛到 50，最老 10 条淘汰；FACT 在非 FACT 不够删之前豁免。
// 阶段 B：60 条 FACT → 非 FACT 没得删 → FACT 也按最老淘汰（容量是硬上限）。
async function scenarioContextPrune() {
  const dirA = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-prune-a-'));
  const storeA = new Store(path.join(dirA, 'state.db'), { maxContextRows: 50 });
  for (let i = 0; i < 60; i++) storeA.addContext('test', 'NOTE', `note-${i}`);
  const cntA = storeA.db.prepare('SELECT COUNT(*) AS n FROM context').get().n;
  check('P4 治理：灌 60 条 NOTE 后收敛到 50', cntA === 50, `n=${cntA}`);
  const textsA = new Set(storeA.db.prepare('SELECT text FROM context').all().map((r) => r.text));
  check('P4 治理：最老 10 条被淘汰', [0, 5, 9].every((i) => !textsA.has(`note-${i}`)));
  check('P4 治理：新条目存活', textsA.has('note-50') && textsA.has('note-59'));
  const prunedA = storeA.eventsAfter(0, 500).filter((e) => e.type === 'context.pruned');
  check('P4 治理：context.pruned 事件记录 removed/kept/max',
    prunedA.length > 0 && prunedA.every((e) => e.payload.kept === 50 && e.payload.max === 50 && e.payload.removed >= 1),
    prunedA.length ? JSON.stringify(prunedA[prunedA.length - 1].payload) : '(none)');

  // FACT 豁免：50 条 NOTE 之上再灌 5 FACT + 10 NOTE → 5 条 FACT 全存活，NOTE 只剩 45 条
  for (let i = 0; i < 5; i++) storeA.addContext('test', 'FACT', `fact-${i}`);
  for (let i = 60; i < 70; i++) storeA.addContext('test', 'NOTE', `note-${i}`);
  const factCnt = storeA.db.prepare("SELECT COUNT(*) AS n FROM context WHERE kind='FACT'").get().n;
  const noteCnt = storeA.db.prepare("SELECT COUNT(*) AS n FROM context WHERE kind='NOTE'").get().n;
  const cntA2 = storeA.db.prepare('SELECT COUNT(*) AS n FROM context').get().n;
  check('P4 治理：FACT 豁免（5/5 存活，只淘汰 NOTE）', factCnt === 5 && noteCnt === 45 && cntA2 === 50,
    `fact=${factCnt} note=${noteCnt} total=${cntA2}`);

  // 阶段 B：全是 FACT 时 FACT 也按最老兜底淘汰
  const dirB = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-prune-b-'));
  const storeB = new Store(path.join(dirB, 'state.db'), { maxContextRows: 50 });
  for (let i = 0; i < 60; i++) storeB.addContext('test', 'FACT', `fact-${i}`);
  const cntB = storeB.db.prepare('SELECT COUNT(*) AS n FROM context').get().n;
  const textsB = new Set(storeB.db.prepare('SELECT text FROM context').all().map((r) => r.text));
  check('P4 治理：全 FACT 时容量仍是硬上限（收敛 50）', cntB === 50, `n=${cntB}`);
  check('P4 治理：FACT 兜底淘汰最老', !textsB.has('fact-0') && !textsB.has('fact-9') && textsB.has('fact-59'));

  storeA.close(); storeB.close();
  fs.rmSync(dirA, { recursive: true, force: true });
  fs.rmSync(dirB, { recursive: true, force: true });
}

// ---------- 场景 14：m5' PATCH_SUMMARY 广播 + m2 ungated trailer ----------
// 合并成功后共享上下文应出现 PATCH_SUMMARY（含文件名、sha、(ungated)），git 历史带 trailer。
async function scenarioPatchSummary() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'good patch', createdBy: 'test' });
  const PORT = 17877;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'patch summary test', testCmd: null, // 无门禁 → ungated
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/good.txt ok',
      'commit good patch',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say done',
    ].join('\n'),
  };
  runner.startIntegrator();
  await runner.spawnWorker('w1');
  await waitFor(() => store.item('t1').status === 'done', { label: 'item done', timeoutMs: 30000 });

  const ps = store.db.prepare("SELECT * FROM context WHERE kind='PATCH_SUMMARY'").all();
  check('PATCH_SUMMARY：广播条目存在', ps.length >= 1, `n=${ps.length}`);
  const txt = ps[0]?.text || '';
  check('PATCH_SUMMARY：含改动文件名', txt.includes('src/good.txt'), txt.slice(0, 160));
  check('PATCH_SUMMARY：含集成 sha 前 10 位', /@[0-9a-f]{10}/.test(txt), txt.slice(0, 80));
  check('PATCH_SUMMARY：无门禁时标记 (ungated)', txt.includes('(ungated)'), txt.slice(0, 120));
  check('PATCH_SUMMARY：item 已 done', store.item('t1').status === 'done');

  // m2：ungated trailer 进了 git 历史
  const lastMsg = git(['log', '-1', '--format=%B', 'swarm/integration'], repoDir);
  check('m2：集成分支最新提交带 Agensh-Integration trailer', /Agensh-Integration:\s*[\w-]+/.test(lastMsg),
    lastMsg.replace(/\n/g, '\\n'));
  check('m2：无 testCmd 时带 Agensh-Ungated: true', /Agensh-Ungated:\s*true/.test(lastMsg));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 19：提交后分支被吸收 → stale 拒绝，item 不记 done（修 #4 归因） ----------
// 复现 tui-demo 事故：item 记 done 但代码没进集成分支（空合并）。w1 提交真实改动；
// 集成器尚未跑时把 w1 分支 reset 回集成基线（等价 P5 吸收效果）——提交的 commit_sha
// 不再是分支祖先 → 集成器必须拒绝（stale）而非空合并记 done。
// 这里由 store 直写入队 submission（带真实 commit_sha），模拟 worker 提交后、集成前的窗口。
async function scenarioStaleSubmitRejected() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'real change that will be absorbed', createdBy: 'test' });
  const PORT = 17872;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'stale submit test', testCmd: null,
    maxTurns: 2, idleTurnsToExit: 99, minWorkers: 0, maxWorkers: 1,
  });
  // 手工造一条带真实 commit_sha 的 queued submission：建 w1 worktree、写文件、提交、入队
  await repo.addWorktree('w1');
  const wDir = path.join(repoDir, '.swarm', 'worktrees', 'w1');
  fs.mkdirSync(path.join(wDir, 'src'), { recursive: true });
  fs.writeFileSync(path.join(wDir, 'src', 'real.txt'), 'real-work\n');
  git(['add', '-A'], wDir);
  git(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', 'real change'], wDir);
  const realSha = git(['rev-parse', 'HEAD'], wDir);
  // 直接落库 submission（绕过 HTTP，锚定真实 commit_sha）
  store.db.prepare("INSERT INTO submissions (id, ts, peer, item_id, branch, commit_sha, state, reason) VALUES (?,?,?,?,?,?,?,?)")
    .run('s-stale-1', Date.now(), 'w1', 't1', 'swarm/w-w1', realSha, 'queued', null);
  store.db.prepare("UPDATE work_items SET status='submitted' WHERE id='t1'").run();
  // 提交入队后：把 w1 分支 reset 回集成基线（等价 backupAndResetWorktree 的吸收效果）
  git(['reset', '--hard', 'swarm/integration'], wDir);
  runner.startIntegrator();
  // 集成器应拒绝（stale），item 退回 pending 而非 done
  await waitFor(() => store.eventsAfter(0, 500).some((e) => e.type === 'integration.stale'),
    { label: 'stale rejected', timeoutMs: 60000 });
  const evs = store.eventsAfter(0, 500);
  check('归因：stale 提交被拒绝（integration.stale 事件）', evs.some((e) => e.type === 'integration.stale'));
  check('归因：item 未记 done（退回 pending 重试）', store.item('t1').status !== 'done', store.item('t1').status);
  check('归因：集成记录 merged=0（无空合并）', store.integrations().some((i) => i.merged === 0));
  check('归因：无空合并 commit 进集成分支', !git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir).includes('src/real.txt'));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

// ---------- 场景 20：集成前清场——worktree 里的 testCmd 副产物不阻塞 merge（修 #2 脏区） ----------
// 复现 .pyc 事故：门禁 testCmd 在集成 worktree 里生成 __pycache__/x.pyc 等未跟踪副产物，
// 若下次 merge 碰到同名 tracked 文件会报 "local changes would be overwritten"。
// integrate() 合并前必须 reset --hard + clean -fd 清场，merge 照常成功。
async function scenarioIntegrationCleanSlate() {
  const repoDir = tmpRepo();
  const repo = new Repo(repoDir); repo.init();
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  store.addItem({ id: 't1', title: 'clean merge', createdBy: 'test' });
  const PORT = 17871;
  const { server } = await createServer(store, { port: PORT });
  const runner = new Runner({ resourceGuard: false,
    store, repoRoot: repoDir, serverPort: PORT, harness: 'echo',
    mission: 'clean slate test', testCmd: null,
    maxTurns: 1, idleTurnsToExit: 1, minWorkers: 1, maxWorkers: 1,
  });
  runner.echoScripts = {
    w1: [
      'claim t1',
      'file src/clean.txt clean-work',
      'commit clean change',
      'api POST /submit {"token":"{{token}}","peer":"{{peer}}"}',
      'say submitted',
    ].join('\n'),
  };
  await runner.spawnWorker('w1');
  await waitFor(() => store.hasQueuedSubmissionForPeer('w1'), { label: 'submission queued', timeoutMs: 60000 });
  // 在集成 worktree 里埋脏区：一个未跟踪副产物 + 一个 tracked 文件的本地改动（模拟 testCmd 留下的 .pyc 与脏改）
  const itDir = repo.ensureIntegrationWorktree();
  fs.mkdirSync(path.join(itDir, '__pycache__'), { recursive: true });
  fs.writeFileSync(path.join(itDir, '__pycache__', 'result_validator.cpython-313.pyc'), 'fake-pyc');
  fs.writeFileSync(path.join(itDir, '.gitignore'), 'DIRTY-LOCAL-CHANGE\n'); // tracked 文件本地改动
  runner.startIntegrator();
  await waitFor(() => store.item('t1').status === 'done', { label: 'merged despite dirty worktree', timeoutMs: 60000 });
  check('清场：脏集成 worktree 不阻塞 merge，item done', store.item('t1').status === 'done');
  check('清场：集成分支含提交的文件', git(['ls-tree', '-r', '--name-only', 'swarm/integration'], repoDir).includes('src/clean.txt'));
  check('清场：未跟踪副产物已被 clean 清掉', !fs.existsSync(path.join(itDir, '__pycache__', 'result_validator.cpython-313.pyc')));
  check('清场：tracked 文件本地改动已被 reset 还原', !String(fs.readFileSync(path.join(itDir, '.gitignore'), 'utf8')).includes('DIRTY-LOCAL-CHANGE'));

  await runner.stop(); server.close(); store.close();
  fs.rmSync(repoDir, { recursive: true, force: true });
}

export async function run() {
  await scenarioGateFail();
  await scenarioGatePass();
  await scenarioAutoscale();
  await scenarioAbort();
  await scenarioLeaseRenew();
  await scenarioBaselineSync();
  await scenarioIntegrationIsolation();
  await scenarioDeepAutoscale();
  await scenarioMinWorkersFloor();
  await scenarioUnlimitedWorkers();
  await scenarioSplitDedup();
  await scenarioJudgeDedup();
  await scenarioReaper();
  await scenarioReaperGuard();
  await scenarioStructureGate();
  await scenarioSyncSkew();
  await scenarioRedoExhausted();
  await scenarioBatchIntegrate();
  await scenarioItemFuse();
  await scenarioBudgetFuse();
  await scenarioContextPrune();
  await scenarioPatchSummary();
  await scenarioStaleSubmitRejected();
  await scenarioIntegrationCleanSlate();
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) throw new Error(`${failed.length} checks failed`);
  return results;
}

if (process.argv[1] && process.argv[1].endsWith('protocol_test.js')) {
  run().then(() => { console.log('protocol_test OK'); process.exit(0); })
    .catch((e) => { console.error('protocol_test FAILED:', e.message); process.exit(1); });
}
