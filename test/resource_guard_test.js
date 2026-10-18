// 系统资源护栏测试：纯函数 + 采样器语义，不依赖 store/git/API，离线可跑。
// 覆盖事故回归：机器超载时必须停止扩容（high）乃至主动缩容（critical），
// 且 Windows 上 os.loadavg() 恒为 0 的坑不能把护栏变成死代码（必须走 CPU 差分）。
import os from 'node:os';
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert';
import { machineCeiling, guardCeiling, makeSampler } from '../src/sysload.js';
import { Store } from '../src/store.js';
import { Repo } from '../src/gitutil.js';
import { Runner } from '../src/runner.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function countActive(runner) {
  return [...runner.workers.values()].filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
}

function check(name, fn) {
  try { fn(); console.log(`  ok  ${name}`); }
  catch (e) { console.error(`  FAIL ${name}: ${e.message}`); throw e; }
}

export async function run() {
  console.log('machineCeiling: CPU 核数的一半，下限 2');
  check('4 核 → 2', () => assert.equal(machineCeiling(4), 2));
  check('8 核 → 4', () => assert.equal(machineCeiling(8), 4));
  check('16 核 → 8', () => assert.equal(machineCeiling(16), 8));
  check('1 核 → 2（下限）', () => assert.equal(machineCeiling(1), 2));
  check('异常输入 → 下限 2', () => assert.equal(machineCeiling(0), 2));

  console.log('guardCeiling: 正常状态不设上限（尊重用户 --max-workers）');
  check('8 核 / cpu 30% / mem 50% / active 10 → ceiling null, ok', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 0.3, freeMemRatio: 0.5 }, { active: 10 });
    assert.equal(r.pressure, 'ok');
    assert.equal(r.ceiling, null);
  });

  console.log('guardCeiling: 高压时只许停不许扩（ceiling = active）');
  check('cpu 85% → high, ceiling=active', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 0.85, freeMemRatio: 0.5 }, { active: 10 });
    assert.equal(r.pressure, 'high');
    assert.equal(r.ceiling, 10);
  });
  check('可用内存 12% → high', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 0.2, freeMemRatio: 0.12 }, { active: 6 });
    assert.equal(r.pressure, 'high');
    assert.equal(r.ceiling, 6);
  });

  console.log('guardCeiling: 临界时主动缩容（ceiling = active - 1）');
  check('cpu 95% → critical, ceiling=active-1', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 0.95, freeMemRatio: 0.5 }, { active: 10 });
    assert.equal(r.pressure, 'critical');
    assert.equal(r.ceiling, 9);
  });
  check('可用内存 5% → critical', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 0.2, freeMemRatio: 0.05 }, { active: 3 });
    assert.equal(r.pressure, 'critical');
    assert.equal(r.ceiling, 2);
  });
  check('active=0 时 critical 仍有下限 1（不出现 0/负数）', () => {
    const r = guardCeiling({ cpus: 8, cpuBusy: 1, freeMemRatio: 0.5 }, { active: 0 });
    assert.equal(r.pressure, 'critical');
    assert.equal(r.ceiling, 1);
  });

  console.log('guardCeiling: 首样本 cpuBusy=null 不能误判');
  check('cpuBusy null + 内存充足 → ok, ceiling null', () => {
    const r = guardCeiling({ cpus: 4, cpuBusy: null, freeMemRatio: 0.6 }, { active: 2 });
    assert.equal(r.pressure, 'ok');
    assert.equal(r.ceiling, null);
  });

  console.log('采样器：首样本 cpuBusy 为 null，第二样本给出 0..1 的实数');
  const sampler = makeSampler();
  const s1 = sampler();
  check('首样本 cpuBusy=null', () => assert.equal(s1.cpuBusy, null));
  check('cpus 与 os.cpus() 一致', () => assert.equal(s1.cpus, os.cpus().length));
  check('freeMemRatio 在 [0,1]', () => assert.ok(s1.freeMemRatio >= 0 && s1.freeMemRatio <= 1));
  // 制造一点 CPU 活动，确保差分窗口内有 times 增量
  const t0 = Date.now(); while (Date.now() - t0 < 30) { /* busy */ }
  const s2 = sampler();
  check('第二样本 cpuBusy 为 [0,1] 实数', () => {
    assert.ok(typeof s2.cpuBusy === 'number');
    assert.ok(s2.cpuBusy >= 0 && s2.cpuBusy <= 1);
  });
  const other = makeSampler();
  check('独立采样器状态互不污染', () => {
    const o1 = other();
    assert.equal(o1.cpuBusy, null); // 新实例不应看到前一个实例的基线
  });

  console.log('事故复现：4 核机器上开了 12 个 worker 且 CPU 打满');
  check('critical 裁决会要求缩容到 11 以下', () => {
    const r = guardCeiling({ cpus: 4, cpuBusy: 0.99, freeMemRatio: 0.4 }, { active: 12 });
    assert.equal(r.pressure, 'critical');
    assert.ok(r.ceiling < 12, `ceiling=${r.ceiling} 应小于 active=12`);
    assert.equal(r.ceiling, 11);
  });

  await scenarioAutoscalerGuard();
}

// 集成场景：注入假系统探针，验证 autoscaler 真的被护栏拦住/放行。
// 不用真实采样——测试机负载不可控，用真实采样会让断言变成碰运气。
async function scenarioAutoscalerGuard() {
  console.log('集成：autoscaler 在 critical 压力下不扩容，压力解除后恢复扩容');
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-guard-'));
  let runner = null, store = null;
  try {
    const repo = new Repo(repoDir); repo.init();
    store = new Store(path.join(repoDir, '.swarm', 'state.db'));
    for (let i = 0; i < 20; i++) store.addItem({ title: `task ${i}`, createdBy: 'test' });

    let probe = { cpus: 12, cpuBusy: 0.99, freeMemRatio: 0.5 }; // critical
    runner = new Runner({
      store, repoRoot: repoDir, serverPort: 0, harness: 'echo', mission: 'guard scenario',
      maxTurns: 99, idleTurnsToExit: 99, minWorkers: 1, maxWorkers: 8,
      resourceGuard: true, sysProbe: () => probe,
    });
    runner.autoscalerIntervalMs = 150;
    await runner.spawnWorker('w1');
    runner.startAutoscaler();
    await sleep(900);

    const underPressure = countActive(runner);
    check(`critical 压力下 active 停在 1（实际 ${underPressure}）`, () => assert.equal(underPressure, 1));
    check('status() 暴露护栏状态', () => {
      const s = runner.status();
      assert.equal(s.resourceGuard.enabled, true);
      assert.equal(s.resourceGuard.pressure, 'critical');
      assert.equal(s.resourceGuard.ceiling, 1);
    });
    check('压力档位变化写入了 autoscaler.guard_pressure 事件', () => {
      const evs = store.eventsByType('autoscaler.guard_pressure', 20);
      assert.ok(evs.length >= 1, 'expected at least one guard event');
    });

    // 压力解除 → autoscaler 应恢复扩容（护栏不是永久刹车）
    probe = { cpus: 12, cpuBusy: 0.2, freeMemRatio: 0.6 };
    await sleep(1600);
    const afterRelief = countActive(runner);
    check(`压力解除后扩容到 >1（实际 ${afterRelief}）`, () => assert.ok(afterRelief > 1));

    await runner.stop();
    runner = null;
    store.close(); store = null;
  } finally {
    try { if (runner) await runner.stop(); } catch { /* 已停 */ }
    try { if (store) store.close(); } catch { /* 已关 */ }
    fs.rmSync(repoDir, { recursive: true, force: true });
  }
}

// 允许直接运行：node test/resource_guard_test.js
if (process.argv[1] && process.argv[1].endsWith('resource_guard_test.js')) {
  run().then(() => { console.log('resource_guard_test OK'); process.exit(0); })
    .catch((e) => { console.error('resource_guard_test FAILED:', e.message); process.exit(1); });
}