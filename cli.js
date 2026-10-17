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
import { Repo, gitOkAsync } from './src/gitutil.js';
import { setMaxInflight, planMission, createDedupJudge } from './src/harness.js';
import { validateRules } from './src/prompts.js';

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
  // 防御：仓库目录是位置参数，若误传成 flag（如 `run --cwd xxx`），args[1] 会是 "--cwd"
  // → 静默在 "./--cwd" 下建出垃圾仓库（真实事故：run3 的 pending=0 之谜）。
  const repoDirArg = (args[1] || '').trim();
  if (['init', 'seed', 'run', 'board', 'status'].includes(cmd) && repoDirArg.startsWith('--')) {
    console.error(`error: <repoDir> must be a positional path, got flag "${repoDirArg}". Usage: swarm ${cmd} <repoDir> [--flags...]`);
    process.exit(2);
  }
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
    // 崩溃续跑：第一次 run 把 mission 元数据写进 store.kv；崩溃后再次 run 同一目录时，
    // 用户不传参数也能从 kv 读回（显式传参优先，会覆盖 kv 里的旧值）。
    const stateDbPath = stateDb(repoDir);
    const kvBootstrap = fs.existsSync(stateDbPath) ? new Store(stateDbPath, {}) : null;
    const fromKv = (k) => { try { return kvBootstrap ? kvBootstrap.getKv(k) : null; } catch { return null; } };
    const mission = flag('mission') || fromKv('mission') || '(no mission given)';
    const testCmd = flag('test-cmd') || fromKv('testCmd') || null;
    const harness = flag('harness') || fromKv('harness') || 'pi';
    const model = flag('model') || fromKv('model') || undefined;
    // --workers 只作为上限别名（max），下限默认 1：autoscaler 按真实任务量伸缩。
    // 若把 --workers 也当下限，会出现"20 个任务强拉 200 worker"的空转（idle_skip 爆炸）。
    const maxWorkers = Number(flag('max-workers') || flag('workers') || fromKv('maxWorkers') || 8);
    const minWorkers = Number(flag('min-workers') || fromKv('minWorkers') || 1);
    // 自动退出模式：board 清空、集成队列空、worker 全退场时优雅退出并打印
    // `[swarm] DONE {json}` 总结行（供 /swarm 扩展解析）。此时 autoscaler 下限
    // 必须降为 0——否则它会按 minWorkers 永远重拉 worker，active 永不为 0。
    const exitWhenDone = !!flag('exit-when-done');
    const maxTurns = Number(flag('max-turns') || fromKv('maxTurns') || 10);
    const leaseSec = Number(flag('lease-sec') || fromKv('leaseSec') || 300);
    const port = Number(flag('port') || 17771);
    const conventions = flag('conventions') ? fs.readFileSync(flag('conventions'), 'utf8') : (fromKv('conventions') || null);
    const seedFile = flag('seed');
    const apiBase = flag('api-base') || process.env.SWARM_API_BASE || fromKv('apiBase') || null;
    const apiKey = flag('api-key') || process.env.SWARM_API_KEY || null;
    // 全局在飞 API 闸门：限制同时打到中转站的请求数（0 = 不限流）。
    // 1000 agent 同时冲会把中转站压死（大面积 503），反而拖长总墙钟时间。
    const maxInflight = flag('max-inflight');
    // 未显式指定时把在飞上限抬到 max(64, worker 数)：让 apiGate 别在 worker 数之下自锁；
    // 真实上限由限流反馈的 AIMD 自动收敛（撞 429 就降 0.75x，稳定就缓慢回升）。
    if (maxInflight !== null && maxInflight !== true) setMaxInflight(maxInflight);
    else setMaxInflight(Math.max(64, maxWorkers));
    const apiToken = null; // 本地运行先不开 token
    // P7 预算熔断 + P6 熔断阈值：均可经 flag 或环境变量配置。
    // budgetTokens=0 表示"立即熔断"（测试用），null 表示不设限。
    const budgetTokensRaw = flag('budget-tokens') ?? process.env.SWARM_BUDGET_TOKENS ?? null;
    const budgetTokens = budgetTokensRaw === null || budgetTokensRaw === true ? null : Number(budgetTokensRaw);
    const maxItemFailsRaw = flag('max-item-fails') ?? process.env.SWARM_MAX_ITEM_FAILS ?? null;
    const maxItemFails = maxItemFailsRaw === null || maxItemFailsRaw === true ? null : Number(maxItemFailsRaw);
    const logFile = flag('log-file') || null;

    new Repo(repoDir).init();
    const store = new Store(stateDb(repoDir), maxItemFails != null ? { maxItemFails } : {});
    // 源码冲突的 LLM 仲裁需要 apiBase/apiKey/model；无配置时 _autoResolveWithLLM 直接返回
    // resolved:false，回退到 merge --abort（行为不变）。
    const repo = new Repo(repoDir, { llmApiBase: apiBase, llmApiKey: apiKey, llmModel: model });
    // prompt 规则冲突检测：RULES 注册表里如果有互相矛盾的规则（如"必须最先做 X" vs "必须最后做 X"），
    // 启动时直接报警，避免 worker 收到冲突指令后行为不可预测。
    const ruleCheck = validateRules();
    if (ruleCheck.warnings.length > 0) {
      console.warn('[swarm] prompt rule warnings:');
      for (const w of ruleCheck.warnings) console.warn(`  - ${w}`);
    }
    if (!ruleCheck.ok) {
      console.error('[swarm] prompt rule conflicts detected:');
      for (const e of ruleCheck.errors) console.error(`  - ${e}`);
      process.exit(1);
    }
    // 持久化 mission 元数据到 kv（仅当用户显式传参或 kv 还没有时；显式传参优先覆盖）。
    // 崩溃后再次 run 不传参数时，从 kv 读回这些值继续 mission。
    const kvPairs = { mission, testCmd, harness, model, maxWorkers, minWorkers, maxTurns, leaseSec, conventions, apiBase };
    for (const [k, v] of Object.entries(kvPairs)) if (v != null) store.setKv(k, typeof v === 'string' ? v : JSON.stringify(v));
    if (kvBootstrap) kvBootstrap.close();
    // --log-file：把 store 事件旁路写成 NDJSON（一行一个 JSON），供 agent tail -f 读。
    let logStream = null;
    if (logFile) {
      logStream = fs.createWriteStream(path.resolve(logFile), { flags: 'a' });
      store.setLogHook((ev) => logStream.write(JSON.stringify(ev) + '\n'));
      console.log(`[swarm] event log → ${path.resolve(logFile)}`);
    }
    // 崩溃恢复（只在 run 启动、尚无 worker 存活时做）：
    // ① 释放上次进程崩溃留下的活跃租约——否则对应工作项要等租约自然过期（默认 300s）才能被重认领；
    // ② 扫集成分支的 integration trailer，补回"已 merge 但没落库"的集成记录（崩溃在 merge 与落库之间）。
    const orphans = store.releaseOrphanClaims();
    if (orphans > 0) console.log(`[swarm] recovered: released ${orphans} orphan claim(s)`);
    // 启动对齐：两次 run 之间 main 可能前进了（用户手工提交/seed 前准备提交），
    // 集成分支停在旧基线会让 worker 看不到新文件、收尾 ff-only 因分叉被拒。
    const align = repo.alignIntegrationToMain();
    if (align.aligned) console.log(`[swarm] integration branch aligned to main (${align.mode})`);
    else if (align.reason && !/up to date|integration ahead/.test(align.reason)) console.warn(`[swarm] integration branch align failed: ${align.reason}`);
    let recovered = [];
    try { recovered = repo.recoverIntegrations(store) || []; } catch (e) { console.error('[swarm] recoverIntegrations failed:', e.message); }
    for (const r of recovered) store.recordIntegration('recover', null, r.sha, null, true, true);
    if (recovered.length > 0) console.log(`[swarm] recovered: backfilled ${recovered.length} integration(s)`);
    if (seedFile) {
      const seed = JSON.parse(fs.readFileSync(seedFile, 'utf8'));
      for (const it of seed.items || []) store.addItem({ ...it, createdBy: 'seed' });
      for (const c of seed.context || []) store.addContext('seed', c.kind || 'FACT', c.text, { itemId: c.itemId, tags: c.tags });
    }
    // --plan：board 无"未完成"工作项且给了 mission 时，自动规划首批工作项（省掉人工 seed.json）。
    // 不能只看 total===0：同一仓库二次 run 时 board 满是上一轮的 done/failed 项，
    // 新 mission 会被静默跳过 → exit-when-done 立即假完成（真实事故：TUI 演示窗口里
    // 第二个任务"你好"和任务D都 done:3/0 apiCalls 秒退）。按"还有没有活"判断：
    const st = store.stats();
    const boardDrained = st.total === 0 || (st.pending + st.inProgress + st.submitted === 0);
    if (flag('plan') && boardDrained && mission && mission !== '(no mission given)') {
      // 动态计算拆解数：根据 mission 长度估算任务量，避免"30 个函数只拆 6 项"的长尾。
      // 经验公式：每 100 字 ~ 5-8 个工作项，下限 6、上限 20（planMission 内部也会 clamp 到 20）。
      const autoCount = Math.max(6, Math.min(20, Math.ceil(mission.length / 15)));
      const planned = await planMission({ mission, conventions: conventions || '', count: Number(flag('plan-count') || autoCount), apiBase, apiKey, model });
      for (const it of planned.items) store.addItem({ ...it, createdBy: 'plan' });
      console.log(`[swarm] planned ${planned.items.length} top-level item(s) from mission`);
      for (const it of planned.items) console.log(`  - ${it.title}`);
    }

    const runner = new Runner({
      store, repoRoot: repoDir, serverPort: port, apiToken,
      harness, model, mission, conventions, testCmd,
      maxTurns, maxWorkers, minWorkers: exitWhenDone ? 0 : minWorkers, leaseSec, apiBase, apiKey,
      budgetTokens,
      // 任务发散护栏：auto-split 的总 item 数上限。默认 200，--max-items 覆盖。
      maxItems: Number(flag('max-items') || 200),
    });
    // LLM 判官（两阶段去重终审）：有中转站凭据才启用；无凭据返回 null，
    // server 走 addItem 原有词面折叠（零配置行为不变）。--no-dedup-judge 可显式关闭。
    const dedupJudge = flag('no-dedup-judge') ? null : createDedupJudge({ apiBase, apiKey, model });
    if (dedupJudge) console.log('[swarm] dedup judge: enabled (LLM final review for duplicate subtasks)');
    const { server } = await createServer(store, {
      port, token: apiToken, getStatus: () => runner.status(), dedupJudge,
      // /submit 时 worker 没带 commitSha 就用分支当前 tip 回填（真实 pi harness 会传，
      // echo/脚本 harness 不传）。归因校验依赖这个 sha 检测"提交后分支被吸收"。
      getCommitSha: (peer) => gitOkAsync(['rev-parse', `swarm/w-${peer}`], repoDir),
    });
    runner.startIntegrator();
    runner.startLeaseRenewer();
    runner.startAutoscaler();
    runner.startReaper();
    // 初始拉起 minWorkers（自动退出模式下仍拉 max(1, minWorkers) 个起活）
    for (let i = 0; i < Math.max(1, minWorkers); i++) await runner.spawnWorker(`w${i + 1}`);
    const startMs = Date.now();
    console.log(`swarm running: harness=${harness} workers=${minWorkers}..${maxWorkers} api=http://127.0.0.1:${port}${exitWhenDone ? ' exit-when-done' : ''}`);
    console.log('press Ctrl+C to stop');
    // 实时运行窗口：5s 一刷，带 tokens/s 差分速度（真实调用才有速度，echo 离线为 0）。
    let lastTk = null; let lastTs = Date.now(); let elapsed = 0;
    const tick = setInterval(() => {
      const s = runner.status();
      const now = Date.now();
      const tk = s.tokenUsage || {};
      let rate = '';
      if (lastTk) {
        const dt = (now - lastTs) / 1000;
        const dOut = (tk.outputTokens || 0) - (lastTk.outputTokens || 0);
        const dIn = (tk.inputTokens || 0) - (lastTk.inputTokens || 0);
        const dCache = (tk.cacheReadTokens || 0) - (lastTk.cacheReadTokens || 0);
        const perSec = dt > 0 ? (dOut + dIn + dCache) / dt : 0;
        rate = ` | ${(perSec).toFixed(0)} tok/s`;
      }
      // 快照必须取原始值：tokenUsage 是 runner 原地修改的同一对象，
      // 存引用会导致差分恒为 0（真实事故：demo 窗口全程 0 tok/s）。
      lastTk = { inputTokens: tk.inputTokens || 0, outputTokens: tk.outputTokens || 0, cacheReadTokens: tk.cacheReadTokens || 0 }; lastTs = now; elapsed = Math.round((now - startMs) / 1000);
      const mm = String(Math.floor(elapsed / 60)).padStart(2, '0'); const ss = String(elapsed % 60).padStart(2, '0');
      const infl = s.apiInflight || {};
      const wparts = s.workers.map((w) => `${w.name}:${w.status}`).join(' ');
      console.log(`[${mm}:${ss}] pending=${s.stats.pending} inprog=${s.stats.inProgress} submtd=${s.stats.submitted} done=${s.stats.done}/${s.stats.total} integ=${(s.integrations || []).length} in-flight=${infl.active ?? '-'}(q${infl.queued ?? 0}) w=[${wparts}]${rate}`);
    }, 5000);

    // P7：预算熔断钩子。runner 检测超限时回调这里走统一优雅停机——
    // 与 'done' 同级取 final 总结（损失如实进 DONE 行：done/total + failed 数）。
    runner.onBudgetExceeded = () => shutdown('budget');

    // 优雅停止的唯一出口：SIGINT 和自动退出共用，保证资源清理路径一致。
    let exiting = false;
    const shutdown = async (reason) => {
      if (exiting) return;
      exiting = true;
      clearInterval(tick);
      clearInterval(doneWatch);
      // 总结快照必须在 store.close() 之前取（close 后查询全抛）
      const final = reason === 'done' || reason === 'budget' ? runner.status() : null;
      console.log(`\nstopping (${reason})...`);
      try {
        const failed = await runner.stop();
        if (failed && failed.length) {
          // 清理失败的 worktree 会残留（磁盘泄漏）；下次 run 的 _nukeWorktree 会自愈，
          // 但必须让用户知道——静默残留曾在 d4 逃过所有监控。
          console.log(`[swarm] cleanup failed for: ${failed.join(', ')} (will self-heal on next run)`);
        }
        // 扫除散置的临时 worktree（agent 在 %TEMP% 建的验证目录等）
        const swept = repo.pruneForeignWorktrees();
        if (swept.pruned > 0) console.log(`[swarm] swept ${swept.pruned} foreign worktree(s)`);
        server.close();
        store.close();
        if (logStream) await new Promise((r) => logStream.end(r));
      } catch (e) { console.error('shutdown error:', e); }
      if (final) {
        // 收尾交付：把集成分支的成果 fast-forward 回 main，让用户在工作区直接看到产出。
        // 不安全（脏区/分叉/非 main 分支）就跳过并提示，integration 分支始终保留。
        let mergeResult = { merged: false, reason: null };
        try { mergeResult = repo.mergeIntegrationToMain(); } catch { /* 合并失败不阻断退出 */ }
        const tok = final.tokenUsage || {};
        const spent = (tok.inputTokens || 0) + (tok.outputTokens || 0)
          + (tok.cacheCreationTokens || 0) + (tok.cacheReadTokens || 0);
        const wallMs = final.startedAt ? Date.now() - final.startedAt : null;
        const integ = final.integrations || [];
        const gated = integ.filter((i) => i.test_cmd); // 跑过门禁的集成
        const gatePassed = gated.filter((i) => i.test_passed).length;
        const summary = {
          done: final.stats.done || 0, total: final.stats.total || 0,
          failed: final.stats.failed || 0,
          submitted: final.stats.submitted || 0, pending: final.stats.pending || 0,
          integrations: integ.length,
          tokenUsage: final.tokenUsage,
          workers: final.workers.length,
          // ---- 效率指标（agent 归因用） ----
          wallTimeMs: wallMs,
          tokensPerSecond: wallMs > 0 ? +(spent / (wallMs / 1000)).toFixed(1) : null,
          tokensPerItem: final.stats.done > 0 ? Math.round(spent / final.stats.done) : null,
          // cacheRead / (input + cacheRead)：前缀缓存命中率（>0.9 说明 prefix 工程生效）
          cacheHitRate: (tok.inputTokens + tok.cacheReadTokens) > 0
            ? +((tok.cacheReadTokens || 0) / (tok.inputTokens + tok.cacheReadTokens)).toFixed(3) : null,
          gatePassRate: gated.length > 0 ? +(gatePassed / gated.length).toFixed(3) : null,
        };
        if (final.budget && final.budget.exceeded) {
          summary.budgetExceeded = true;
          console.log(`[swarm] budget exhausted: ${final.budget.spent} / ${final.budget.tokens} tokens — open items failed and exit`);
        }
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
