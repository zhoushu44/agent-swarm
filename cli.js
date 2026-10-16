#!/usr/bin/env node
// swarm CLI —— 入口。用法：
//   swarm init <repoDir>                       初始化（git init + 集成分支）
//   swarm seed <repoDir> --file <json>         播种工作项（规格引导产物 → board 种子）
//   swarm seed <repoDir> --mission "..."       同上，但由 LLM 自动把 mission 拆成首批工作项
//   swarm run <repoDir> --mission "..." [--test-cmd "..."] [--harness claude] [--max-workers 8]
//   swarm board <repoDir>                      列出全部工作项
//   swarm status <repoDir>                     汇总状态（进度/租约/待集成/最近集成）
// 状态存到 <repoDir>/.swarm/state.db
import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { Store } from './src/store.js';
import { createServer } from './src/server.js';
import { Runner } from './src/runner.js';
import { Repo } from './src/gitutil.js';
import { setMaxInflight, planMission, createDedupJudge } from './src/harness.js';

const args = process.argv.slice(2);
const cmd = args[0];

function flag(name, def = null) {
  const i = args.indexOf('--' + name);
  if (i === -1) return def;
  const v = args[i + 1];
  return v === undefined || v.startsWith('--') ? true : v;
}

const stateDb = (repoDir) => path.join(repoDir, '.swarm', 'state.db');

async function main() {
  if (cmd === 'init') {
    const repoDir = path.resolve(args[1]);
    new Repo(repoDir).init();
    console.log('initialized:', repoDir, 'integration branch: swarm/integration');
    return;
  }

  if (cmd === 'seed') {
    const repoDir = path.resolve(args[1]);
    const file = flag('file');
    const mission = flag('mission');
    // 缺口⑨：以前必须人工写 seed.json。现在 `seed <repo> --mission "..."` 用一次
    // LLM 调用把 mission 翻译成首批顶层工作项，实现"给一句话就能开跑"。
    let seed;
    if (file && file !== true) {
      seed = JSON.parse(fs.readFileSync(file, 'utf8'));
    } else if (mission && mission !== true) {
      const conventions = flag('conventions') ? fs.readFileSync(flag('conventions'), 'utf8') : '';
      const planned = await planMission({
        mission, conventions,
        count: Number(flag('count') || 6),
        apiBase: flag('api-base') || process.env.SWARM_API_BASE || null,
        apiKey: flag('api-key') || process.env.SWARM_API_KEY || null,
        model: flag('model') || undefined,
      });
      seed = { items: planned.items, context: [] };
      console.log(`planned ${seed.items.length} top-level item(s) from mission`);
      for (const it of seed.items) console.log(`  - ${it.title}`);
    } else {
      console.error('seed: need --file <json> or --mission "<text>"');
      process.exit(1);
    }
    const store = new Store(stateDb(repoDir));
    for (const it of seed.items || []) {
      store.addItem({ ...it, createdBy: 'seed' });
    }
    for (const c of seed.context || []) {
      store.addContext('seed', c.kind || 'FACT', c.text, { itemId: c.itemId, tags: c.tags });
    }
    console.log(`seeded ${seed.items?.length || 0} items, ${seed.context?.length || 0} context entries`);
    store.close();
    return;
  }

  if (cmd === 'run') {
    const repoDir = path.resolve(args[1]);
    const mission = flag('mission') || '(no mission given)';
    const testCmd = flag('test-cmd') || null;
    const harness = flag('harness') || 'pi';
    const model = flag('model') || undefined;
    const maxWorkers = Number(flag('max-workers') || 8);
    const minWorkers = Number(flag('min-workers') || 1);
    // 自动退出模式：board 清空、集成队列空、worker 全退场时优雅退出并打印
    // `[swarm] DONE {json}` 总结行（供 /swarm 扩展解析）。此时 autoscaler 下限
    // 必须降为 0——否则它会按 minWorkers 永远重拉 worker，active 永不为 0。
    const exitWhenDone = !!flag('exit-when-done');
    const maxTurns = Number(flag('max-turns') || 10);
    const leaseSec = Number(flag('lease-sec') || 300);
    const port = Number(flag('port') || 17771);
    const conventions = flag('conventions') ? fs.readFileSync(flag('conventions'), 'utf8') : null;
    const seedFile = flag('seed');
    const apiBase = flag('api-base') || process.env.SWARM_API_BASE || null;
    const apiKey = flag('api-key') || process.env.SWARM_API_KEY || null;
    // 全局在飞 API 闸门：限制同时打到中转站的请求数（0 = 不限流）。
    // 1000 agent 同时冲会把中转站压死（大面积 503），反而拖长总墙钟时间。
    const maxInflight = flag('max-inflight');
    if (maxInflight !== null && maxInflight !== true) setMaxInflight(maxInflight);
    const apiToken = null; // 本地运行先不开 token

    new Repo(repoDir).init();
    const store = new Store(stateDb(repoDir));
    // 崩溃恢复（只在 run 启动、尚无 worker 存活时做）：
    // ① 释放上次进程崩溃留下的活跃租约——否则对应工作项要等租约自然过期（默认 300s）才能被重认领；
    // ② 扫集成分支的 integration trailer，补回"已 merge 但没落库"的集成记录（崩溃在 merge 与落库之间）。
    const orphans = store.releaseOrphanClaims();
    if (orphans > 0) console.log(`[swarm] recovered: released ${orphans} orphan claim(s)`);
    const repo = new Repo(repoDir);
    let recovered = [];
    try { recovered = repo.recoverIntegrations(store) || []; } catch (e) { console.error('[swarm] recoverIntegrations failed:', e.message); }
    for (const r of recovered) store.recordIntegration('recover', null, r.sha, null, true, true);
    if (recovered.length > 0) console.log(`[swarm] recovered: backfilled ${recovered.length} integration(s)`);
    if (seedFile) {
      const seed = JSON.parse(fs.readFileSync(seedFile, 'utf8'));
      for (const it of seed.items || []) store.addItem({ ...it, createdBy: 'seed' });
      for (const c of seed.context || []) store.addContext('seed', c.kind || 'FACT', c.text, { itemId: c.itemId, tags: c.tags });
    }
    // --plan：board 为空且给了 mission 时，先自动规划首批工作项再开跑（省掉人工 seed.json）。
    if (flag('plan') && store.stats().total === 0 && mission && mission !== '(no mission given)') {
      const planned = await planMission({ mission, conventions: conventions || '', count: Number(flag('plan-count') || 6), apiBase, apiKey, model });
      for (const it of planned.items) store.addItem({ ...it, createdBy: 'plan' });
      console.log(`[swarm] planned ${planned.items.length} top-level item(s) from mission`);
      for (const it of planned.items) console.log(`  - ${it.title}`);
    }

    const runner = new Runner({
      store, repoRoot: repoDir, serverPort: port, apiToken,
      harness, model, mission, conventions, testCmd,
      maxTurns, maxWorkers, minWorkers: exitWhenDone ? 0 : minWorkers, leaseSec, apiBase, apiKey,
    });
    // LLM 判官（两阶段去重终审）：有中转站凭据才启用；无凭据返回 null，
    // server 走 addItem 原有词面折叠（零配置行为不变）。--no-dedup-judge 可显式关闭。
    const dedupJudge = flag('no-dedup-judge') ? null : createDedupJudge({ apiBase, apiKey, model });
    if (dedupJudge) console.log('[swarm] dedup judge: enabled (LLM final review for duplicate subtasks)');
    const { server } = await createServer(store, { port, token: apiToken, getStatus: () => runner.status(), dedupJudge });
    runner.startIntegrator();
    runner.startLeaseRenewer();
    runner.startAutoscaler();
    runner.startReaper();
    // 初始拉起 minWorkers（自动退出模式下仍拉 max(1, minWorkers) 个起活）
    for (let i = 0; i < Math.max(1, minWorkers); i++) await runner.spawnWorker(`w${i + 1}`);
    console.log(`swarm running: harness=${harness} workers=${minWorkers}..${maxWorkers} api=http://127.0.0.1:${port}${exitWhenDone ? ' exit-when-done' : ''}`);
    console.log('press Ctrl+C to stop');
    const tick = setInterval(() => {
      const s = runner.status();
      console.log(`[swarm] pending=${s.stats.pending} done=${s.stats.done} workers=${s.workers.map((w) => `${w.name}:${w.status}`).join(' ')}`);
    }, 15000);

    // 优雅停止的唯一出口：SIGINT 和自动退出共用，保证资源清理路径一致。
    let exiting = false;
    const shutdown = async (reason) => {
      if (exiting) return;
      exiting = true;
      clearInterval(tick);
      clearInterval(doneWatch);
      // 总结快照必须在 store.close() 之前取（close 后查询全抛）
      const final = reason === 'done' ? runner.status() : null;
      console.log(`\nstopping (${reason})...`);
      try {
        await runner.stop();
        server.close();
        store.close();
      } catch (e) { console.error('shutdown error:', e); }
      if (final) {
        // 收尾交付：把集成分支的成果 fast-forward 回 main，让用户在工作区直接看到产出。
        // 不安全（脏区/分叉/非 main 分支）就跳过并提示，integration 分支始终保留。
        let mergeResult = { merged: false, reason: null };
        try { mergeResult = repo.mergeIntegrationToMain(); } catch { /* 合并失败不阻断退出 */ }
        const summary = {
          done: final.stats.done || 0, total: final.stats.total || 0,
          submitted: final.stats.submitted || 0, pending: final.stats.pending || 0,
          integrations: (final.integrations || []).length,
          tokenUsage: final.tokenUsage,
          workers: final.workers.length,
        };
        if (mergeResult.merged) {
          summary.mergedToMain = true;
          console.log(`[swarm] merged swarm/integration → main (${(mergeResult.sha || '').slice(0, 8)})`);
        } else {
          console.log(`[swarm] integration branch kept (not merged to main: ${mergeResult.reason || 'unknown'})`);
        }
        console.log('[swarm] DONE ' + JSON.stringify(summary));
      }
      process.exit(0);
    };

    // 自动退出监视：board 清空 + 集成队列空 + 无活跃 worker → 干完活了，退出。
    // 注意 openCount 含 submitted（等门禁的也算活），所以无需单看 submitted；
    // queuedSubmissions 为空保证集成器把最后的提交都 merge 完了。
    let doneWatch = null;
    if (exitWhenDone) {
      doneWatch = setInterval(async () => {
        if (exiting) return;
        try {
          const open = store.openCount();
          const queued = store.queuedSubmissions().length;
          const active = runner.status().active;
          if (open === 0 && queued === 0 && active === 0) await shutdown('done');
        } catch { /* store 已关闭 */ }
      }, 3000);
    }

    process.on('SIGINT', () => shutdown('sigint'));
    return;
  }

  if (cmd === 'board') {
    const repoDir = path.resolve(args[1]);
    const store = new Store(stateDb(repoDir));
    console.table(store.items().map((i) => ({ id: i.id, status: i.status, title: i.title.slice(0, 40) })));
    store.close();
    return;
  }

  if (cmd === 'status') {
    const repoDir = path.resolve(args[1]);
    const store = new Store(stateDb(repoDir));
    const stats = store.stats();
    const open = store.openCount();
    const claimable = store.claimableCount();
    const queued = store.queuedSubmissions();
    const integrations = store.recentIntegrations(10);
    const activeClaims = store.activeClaims();
    const doneRatio = stats.total > 0 ? ((stats.done / stats.total) * 100).toFixed(1) : '0.0';
    console.log(`board: ${repoDir}`);
    console.log(
      `items total=${stats.total} done=${stats.done} (${doneRatio}%) ` +
      `pending=${stats.pending} in_progress=${stats.inProgress} submitted=${stats.submitted}`
    );
    console.log(
      `open=${open} claimable=${claimable} active_claims=${activeClaims.length} ` +
      `queued_submissions=${queued.length} integrations(logged)=${store.integrations().length}`
    );
    if (activeClaims.length > 0) {
      console.log('\nactive claims:');
      console.table(activeClaims.map((c) => ({
        peer: c.peer, itemId: c.item_id,
        expiresIn: `${Math.max(0, Math.round((c.expires_at - Date.now()) / 1000))}s`,
      })));
    }
    if (queued.length > 0) {
      console.log('\nqueued submissions (waiting on integration gate):');
      console.table(queued.map((s) => ({ id: s.id, peer: s.peer, itemId: s.item_id, sha: (s.commit_sha || '').slice(0, 8) })));
    }
    if (integrations.length > 0) {
      console.log('\nrecent integrations:');
      console.table(integrations.map((i) => ({
        ts: new Date(i.ts).toLocaleTimeString(), peer: i.peer, itemId: i.item_id,
        passed: !!i.test_passed, merged: !!i.merged, sha: (i.commit_sha || '').slice(0, 8),
      })));
    }
    // 供脚本解析的机器可读行（与 run 的 DONE 行同风格）
    console.log('[swarm] STATUS ' + JSON.stringify({ ...stats, open, claimable, activeClaims: activeClaims.length, queued: queued.length }));
    store.close();
    return;
  }

  console.log(`swarm — decentralized multi-agent software builder

usage:
  swarm init <repoDir>
  swarm seed <repoDir> --file seed.json
  swarm seed <repoDir> --mission "..." [--count N] [--conventions FILE]   (自动规划首批工作项)
  swarm run <repoDir> --mission "..." [--test-cmd "..."] [--harness pi|direct|claude|codex|echo]
            [--model NAME] [--max-workers N] [--min-workers N] [--max-turns N]
            [--max-inflight N] [--plan] [--port 17771] [--conventions FILE] [--seed FILE]
  swarm board <repoDir>
  swarm status <repoDir>

harness 说明：
  pi      pi CLI（默认，推荐底座）：pi -p 无会话回合，工具 read/bash/edit/write
          （Windows 另有 powershell），离线 flag 全开，状态全在 swarm store。
          认证用 pi auth 或 ANTHROPIC_API_KEY 等环境变量。
  direct  直连 API：绕过 CLI，省掉每回合 ~4400 tokens 的 CLI 系统提示词，
          成本最低，200-1000 agent 首选。读 ~/.claude/settings.json 或环境变量取 token。
  claude  claude -p（每回合起 CLI 进程，prompt 开销大）
  codex   codex exec（需登录）
  echo    离线脚本（不调 API，测试用）`);
}

main().catch((e) => { console.error(e); process.exit(1); });
