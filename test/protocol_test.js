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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  const runner = new Runner({
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

// ---------- 场景 12：拆解幂等按"归一化交付物名"折叠（重复拆解是收敛杀手） ----------
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
  const runner = new Runner({
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
  const runner = new Runner({
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
  await scenarioSplitDedup();
  await scenarioJudgeDedup();
  await scenarioReaper();
  await scenarioReaperGuard();
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) throw new Error(`${failed.length} checks failed`);
  return results;
}

if (process.argv[1] && process.argv[1].endsWith('protocol_test.js')) {
  run().then(() => { console.log('protocol_test OK'); process.exit(0); })
    .catch((e) => { console.error('protocol_test FAILED:', e.message); process.exit(1); });
}
