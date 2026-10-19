// Runner —— swarm 的"手脚"：管理 worker 进程生命周期 + 集成器。
// 关键架构决定（对应你否定的批同步架构）：
//   - 每个 worker 是独立的 async 循环：自己拉任务、自己干、自己提交。
//     runner 不给 worker 分配任务（无 lead 在关键路径上），只负责拉起/回收。
//   - 集成器单独跑：worker submit → store 记录 → 集成器按序合并到集成分支。
//     合并串行（git 语义要求），但跑门禁测试不阻塞 worker 继续干活。
//   - autoscaler 独立循环：只看 board 计数，伸缩 worker 池。
import path from 'node:path';
import fs from 'node:fs';
import crypto from 'node:crypto';
import { Repo, gitAsync, gitOkAsync, isAncestor } from './gitutil.js';
import { getHarness, apiInflight } from './harness.js';
import { buildWorkerPrompt } from './prompts.js';
import { sampleSystem, guardCeiling } from './sysload.js';

export class Runner {
  constructor({ store, repoRoot, serverPort, apiToken, harness = 'claude', model, mission, conventions, testCmd, maxTurns = 10, turnTimeoutMs = 900000, idleTurnsToExit = 2, maxWorkers = 8, minWorkers = 1, leaseSec = 300, worktreeCache = new Map(), apiBase = null, apiKey = null, briefEnabled = true, budgetTokens = null, maxItems = 200, resourceGuard = true, sysProbe = null }) {
    this.store = store; this.repoRoot = repoRoot; this.serverPort = serverPort; this.apiToken = apiToken;
    this.harnessName = harness; this.harness = getHarness(harness); this.model = model;
    this.apiBase = apiBase; this.apiKey = apiKey;
    this.mission = mission; this.conventions = conventions; this.testCmd = testCmd;
    this.maxTurns = maxTurns; this.turnTimeoutMs = turnTimeoutMs; this.idleTurnsToExit = idleTurnsToExit;
    this.maxWorkers = maxWorkers; this.minWorkers = minWorkers;
    // 系统资源护栏：autoscaler 扩容前先看机器脸色（CPU 差分占用率 + 可用内存比）。
    // 事故背景：--max-workers 12 跑在普通 PC 上把机器卡死——项目此前零资源感知。
    // sysProbe 是测试注入口（替代真实采样）；resourceGuard=false 即 --no-resource-guard。
    this.resourceGuard = resourceGuard !== false;
    this.sysProbe = sysProbe || null;
    this._guardState = { pressure: 'ok', ceiling: null, sample: null };
    this.leaseSec = leaseSec;
    // P7 预算熔断：整个 run 的 token 总预算（null = 不设限）。口径 = input+output+
    // cacheCreation+cacheRead 全记——KV-cache 便宜但不是免费，防的是"预算形同虚设"。
    // 200-1000 agent 场景没有预算闸门时，一个配置错误的 run 可以烧穿真金白银。
    this.budgetTokens = budgetTokens != null ? Math.max(0, Number(budgetTokens) || 0) : null;
    this.budgetExceeded = false;
    // briefEnabled=false 时关闭"回合简报"（A/B 对照用）：worker 每回合自己拉全量。
    this.briefEnabled = briefEnabled !== false;
    // token 记账：所有 worker 所有回合的累计（省 token 实验的测量口）。
    this.tokenUsage = { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0, turns: 0, steps: 0 };
    // 按工作项归因的 token（key=item_id）：监控面板用差分算每项实时 tok/s。
    // 没有租约的回合（拆解/idle/重试）只进全局，不硬摊。
    this.itemTokens = new Map();
    // 源码冲突的 LLM 仲裁需要 apiBase/apiKey/model；无配置时 _autoResolveWithLLM 直接返回
    // resolved:false，回退到 merge --abort（行为不变）。
    this.repo = new Repo(repoRoot, { llmApiBase: apiBase, llmApiKey: apiKey, llmModel: model });
    this.workers = new Map(); // name → { name, dir, branch, proc, idleCount, status }
    this.integratorRunning = false;
    this._stopped = false;
    this._nameCounter = 0;
    // 中止通道：stop() 时 abort，正在跑的 harness 子进程立刻被杀（否则 drain 会卡到 turnTimeoutMs）
    this._abort = new AbortController();
    this._startedAt = Date.now();
    this._peakActive = 0;
    // 单任务死磕熔断：worker 连续持有同一 item 超过 maxItemTurns 回合时强制放弃租约。
    // 压测实证：w16 在 src/generator.js 上连跑 8 回合烧 78 万 tokens 仍不完工——
    // 超复杂单文件是单 worker 能力天花板，死磕不如放弃让别的 worker 重试。
    this.maxItemTurns = Number(this.maxItemTurns || 6);
    // P0 动态负载均衡：慢任务检测阈值（毫秒）。持有同一 item 超过此时长且 token 速率
    // 低于平均值一半 → 强制拆分，不等 6 回合。
    this.slowItemMs = Number(this.slowItemMs || 300000); // 5 分钟
    // P6 返工循环检测：worker 持续高 token 输出但长时间无集成 → 判定为返工循环。
    this.reworkThresholdMs = Number(this.reworkThresholdMs || 600000); // 10 分钟
    // 任务发散护栏：auto-split 在总 item 数达到 maxItems 后停止拆解（仅熔断释放）。
    // 见 worker 循环里的 item_split_capped 事件。
    this.maxItems = Number(maxItems || 200);
    this.log = (type, payload) => { try { store.log('runner', type, payload); } catch { /* store 已关闭，忽略 */ } };
    // stdout 镜像：关键事件打一行单行 JSON，agent 不用轮询 /status 也能实时感知。
    // 前缀 [swarm] EVENT 供 grep 过滤；事件集合刻意收敛（门禁/预算/退场），避免刷屏。
    this._emit = (type, payload) => {
      try { process.stdout.write(`[swarm] EVENT ${JSON.stringify({ type, ...payload })}\n`); } catch { /* stdout 不可写时忽略 */ }
    };
  }

  // ---------- worker 生命周期 ----------
  async spawnWorker(name = null) {
    if (!name) name = this._nextName();
    if (this.workers.has(name)) return this.workers.get(name);
    // 单个 worker 拉起失败不炸整条链：初始 spawn 靠它保住 run，autoscaler 批量
    // 扩容靠它保住循环。失败如实记录（下次扩容会重试同名/新名）。
    let wt;
    try { wt = await this.repo.addWorktree(name); }
    catch (e) {
      this.log('worker.spawn_failed', { name, error: String(e && e.message || e).slice(0, 300) });
      return null;
    }
    const w = {
      name, dir: wt.dir, branch: wt.branch, proc: null, idleCount: 0, status: 'starting', turn: 0,
      // 不再在 spawn 时同步 git rev-parse（autoscaler 一轮可能连拉 8 个 worker，
      // 8 × ~100ms 会冻住事件循环）。该值仅用于 worker.new_commit 日志，置 null 即可。
      lastIntegratedHead: null,
      lastDigestTs: 0, lastReply: '', // 回合简报用：上次 digest 时间 + 上回合摘要
      syncSkew: 0, // m4'：基线刷新冲突连续计数。每次 syncWorktree 冲突 +1、成功归零。
      // ≥3 说明该 worker 与集成分支已实质分叉（每回合都在跟别人撞同一批文件），
      // 继续烧回合只是浪费 API——挂起长睡等集成分支稳定；≥8 直接退场（autoscaler 会补新人）。
    };
    this.workers.set(name, w);
    this.log('worker.spawned', { name, dir: wt.dir });
    w.loop = this._runWorkerLoop(w).catch((e) => {
      w.status = 'crashed';
      const category = this._classifyError(e);
      this.log('worker.crashed', { name, category, error: String(e && e.message || e).slice(0, 400) });
    });
    return w;
  }

  // worker crash 分类：把无结构的 Error 消息聚类成可统计的桶。
  // 大规模跑 100+ worker 时，"挂了 5 个"没用，要知道是"5 个全是 API 超时"还是
  // "5 个全是 git 冲突"——前者说明中转站不稳，后者说明任务拆解重叠。
  _classifyError(e) {
    const msg = String(e && e.message || e).toLowerCase();
    const code = String(e && e.code || '').toUpperCase();
    // API/网络层
    if (/\b(etimedout|econnreset|econnrefused|enotfound|eai_again|socket hang up)\b/.test(msg)
      || /\b(etimedout|econnreset|econnrefused)\b/.test(code)) return 'network';
    if (/\b(429|rate.?limit|too many requests|quota exceeded|insufficient.?quota)\b/.test(msg)) return 'rate_limit';
    if (/\b(401|403|unauthorized|forbidden|invalid api key|authentication)\b/.test(msg)) return 'auth';
    if (/\b(500|502|503|504|internal server error|bad gateway|service unavailable)\b/.test(msg)) return 'api_5xx';
    if (/\b(timeout|timed out)\b/.test(msg) && /turn|api|request/.test(msg)) return 'timeout';
    // git/文件系统层
    if (/\b(enospc|no space left)\b/.test(msg)) return 'disk_full';
    if (/\b(eperm|eacces|permission denied)\b/.test(msg)) return 'permission';
    if (/git (?:merge|worktree|checkout|reset)/.test(msg) || /merge conflict|could not merge/.test(msg)) return 'git';
    // harness/协议层
    if (/unknown harness|missing api token|harness.*not.*found/.test(msg)) return 'config';
    if (/parse|json|unexpected token|invalid response/.test(msg)) return 'parse';
    // 兜底
    return 'unknown';
  }
  // 生成不与已有 worker 撞名的名字（初始 w1..wN 由 cli 拉起，autoscaler 从 N+1 续）
  _nextName() {
    let n;
    do { this._nameCounter += 1; n = `w${this._nameCounter}`; } while (this.workers.has(n));
    return n;
  }

  async _runWorkerLoop(w) {
    try {
      while (!this._stopped) {
        // 退场检查必须在最前：否则下面把 status 改回 working 会覆盖 retire 标记
        if (w.status === 'retired' || w.status === 'crashed') break;
        // 空转抑制（第一性原理）：worker 若"手头无租约"且"board 上没有任何可认领项"，
        // 跑一个 LLM 回合的唯一结果就是模型回一句 IDLE —— 一次真实 API 往返（数秒 + 真金白银）
        // 换来零信息。冷启动阶段（子项还没被拆出来）最致命：N 个 worker 同时空转烧回合。
        // 所以先查一个轻量计数（走索引），抢不到活就直接退避轮询，不烧回合。
        // echo harness 是脚本化离线测试，每回合都要按脚本执行、且断言依赖回合推进，故不适用。
        if (this.harnessName !== 'echo') {
          let claimable = 0, mine = 0, open = 0, total = 0;
          try {
            claimable = this.store.claimableCount();
            mine = this.store.activeClaimsByPeer(w.name).length;
            open = this.store.openCount();
            total = this.store.stats().total;
          } catch { /* store 已关闭，忽略 */ }
          // 冷启动豁免：board 完全为空（total=0）时必须烧一个回合让 worker 建第一批
          // 工作项——否则空转抑制会把启动永久堵死（worker 秒退、0/0 收场）。
          const coldStart = total === 0;
          if (claimable === 0 && mine === 0 && !coldStart) {
            w.status = 'idle';
            w.idleCount += 1;
            this.log('worker.idle_skip', { name: w.name, count: w.idleCount, open });
            // board 上真的没活了才退场；否则只等待——退场会让 autoscaler 立刻重建 worktree（重操作）。
            // 但保留下限：池里活跃数已降到 minWorkers 时不再退——否则退一个、autoscaler 又拉一个，
            // 永远 churn（每 ~10s 建一个 worktree 的死循环，1000-agent 压测会烧穿磁盘 IO）。
            // exit-when-done 模式 minWorkers=0，不受此守卫影响，照常退场。
            const activeCount = this._activeCount();
            if (open === 0 && w.idleCount >= this.idleTurnsToExit && activeCount > this.minWorkers) {
              w.status = 'retired';
              this.log('worker.retired_idle', { name: w.name });
              break;
            }
            await sleep(this.idlePollMs || 1000);
            continue;
          }
        }
        // P7 预算闸门：每回合开工前检查。超限 → 剩余开放项全部熔断为 failed（损失如实
        // 呈现，不静默蒸发）+ 标记 budgetExceeded + 触发优雅停机（走 DONE 收尾总结）。
        // 检查放在 idle 跳过之后、turn+1 之前：idle 轮询不烧 token，先退出 idle 再判预算。
        if (this.budgetTokens != null && !this.budgetExceeded && this._tokensSpent() > this.budgetTokens) {
          this.budgetExceeded = true;
          this.log('budget.exceeded', { spent: this._tokensSpent(), budget: this.budgetTokens });
          this._emit('budget.exceeded', { spent: this._tokensSpent(), budget: this.budgetTokens });
          let failedN = 0;
          try { failedN = this.store.failAllOpenItems('runner', 'budget exhausted'); } catch { /* store 已关闭 */ }
          this.log('budget.circuit_broken', { failedItems: failedN });
          // 优雅停机而非硬杀：让 cli 的 shutdown 流程取 final 快照、回并 main、打 DONE 总结。
          // budgetStop 钩子由 cli 注入（调 shutdown('budget')），未注入时退化为软停。
          if (typeof this.onBudgetExceeded === 'function') { try { this.onBudgetExceeded(); } catch { /* 钩子异常不阻断 */ } }
          else this._stopped = true;
          break;
        }
        w.turn += 1;
        // maxTurns 是"软上限"：只有在没有剩余工作时才据此退场。
        // 硬退场会导致 autoscaler 立刻重建 worktree（churn），把并行度浪费在重建上。
        // 但仍设一个硬上限（软上限 ×hardTurnFactor），防止 worker 在持续失败时无限循环。
        const hardMax = this.maxTurns * (this.hardTurnFactor || 6);
        if (w.turn > hardMax) { w.status = 'retired'; this.log('worker.retired_hard', { name: w.name, turns: w.turn - 1 }); break; }
        if (w.turn > this.maxTurns && this._noOpenWork()) { w.status = 'retired'; this.log('worker.retired', { name: w.name, turns: w.turn - 1 }); break; }
        w.status = 'working';
        // 刷新基线：把集成分支最新进展 merge 进本 worktree，避免基线陈旧导致大面积冲突。
        // 有冲突则跳过本回合刷新（不阻塞 worker），下回合再试。
        // 纪元短路：集成分支自上次刷新以来没动过（单进程里只有集成器会推进它）→ 无需任何 git 调用。
        const synced = await this.repo.syncWorktree(w.name, w.lastSyncEpoch ?? null);
        // 冲突时绝不能记 epoch：记了的话下回合纪元短路会跳过 merge 重试，skew 永远停在 1，
        // m4' 的挂起/退场机制就失效了。冲突 → 下回合必须真实重试 merge（吸收最新进度）。
        if (synced.epoch !== undefined && !synced.conflict) w.lastSyncEpoch = synced.epoch;
        if (synced.conflict) {
          // m4' 冲突重试吸收：不再静默放弃。记 skew 计数，把冲突文件透给日志；
          // 连续 ≥8 → 彻底退场（worktree 分叉已不可救，autoscaler 会补新 worker 带新基线进场）。
          w.syncSkew += 1;
          this.log('worker.sync_conflict', { name: w.name, skew: w.syncSkew, files: synced.conflictFiles || [] });
          if (w.syncSkew >= 8) {
            w.status = 'retired';
            this.log('worker.retired_skew', { name: w.name, skew: w.syncSkew });
            break;
          }
          if (w.syncSkew >= 3) {
            // P5 冲突吸收（替代旧"挂起长睡"）：连续 3 回合撞同一批文件说明分叉已实质化，
            // 挂起只是拖延——分叉不会自己消失。改为：备份分叉线 → reset 回集成分支 →
            // 广播 CONFLICT 上下文（下回合简报必带，模型据此避开热点文件）→ 重做。
            // 重做预算上限 2 次（maxConflictRedo）：reset 是重置不是修复，无限重做
            // 等于把同一批文件反复拱手让人。超预算退场，autoscaler 补新人。
            if ((w.redoCount || 0) >= (this.maxConflictRedo ?? 2)) {
              w.status = 'retired';
              this.log('worker.retired_redo_exhausted', { name: w.name, skew: w.syncSkew, redos: w.redoCount || 0 });
              break;
            }
            w.redoCount = (w.redoCount || 0) + 1;
            const files = synced.conflictFiles || [];
            const bs = await this.repo.backupAndResetWorktree(w.name);
            if (bs.ok) {
              w.syncSkew = 0; // 基线已重置，冲突计数归零重新观察
              // 纪元对齐：reset 后基线就是集成分支当前头，无需再 sync。
              // 不记的话下回合会做一次多余（但必然成功）的 merge，无害但浪费。
              w.lastSyncEpoch = this.repo.integrationEpoch;
              // CONFLICT 上下文：进共享 context 表 → 自己和别人的下回合简报都会带上。
              // 这是把"冲突"从纯损失转成"全队绕开热点文件"的信号（Agensh 冲突吸收语义）。
              try {
                this.store.addContext(w.name, 'CONFLICT',
                  `worker ${w.name} reset to integration baseline (redo #${w.redoCount}, backup=${bs.backup || 'none'}). HOT FILES others are actively changing — avoid editing these until context clears: ${files.slice(0, 12).join(', ')}${files.length > 12 ? ` (+${files.length - 12} more)` : ''}`,
                  { tags: ['conflict', 'absorb'] });
              } catch { /* store 已关闭 */ }
              this.log('worker.conflict_absorbed', { name: w.name, redo: w.redoCount, backup: bs.backup, files: files.slice(0, 10) });
            } else {
              // reset 失败（worktree 损坏等）：退场比挂起重试更诚实，reaper 会收尸。
              w.status = 'retired';
              this.log('worker.retired_reset_failed', { name: w.name, reason: bs.reason });
              break;
            }
          }
        } else if (synced.ok) {
          w.syncSkew = 0; // 刷新成功 → 冲突态解除
        }
        // 回合简报（论文的"捎带推送"）：把 agent 缺席期间的变化直接注入 prompt，
        // 省掉它每回合开头的三连拉（board/messages/context）。kv 缓存也友好：简报放 prompt 尾部。
        let brief = '';
        if (this.briefEnabled) {
          try {
            const d = this.store.digestFor(w.name, w.lastDigestTs, 30);
            w.lastDigestTs = d.now;
            brief = this._buildBrief(w, d);
          } catch { /* store 已关闭，跳过简报 */ }
        }
        const prompt = buildWorkerPrompt({
          peerName: w.name, serverPort: this.serverPort, token: this.apiToken,
          mission: this.mission, conventions: this.conventions, turn: w.turn, brief,
          harness: this.harnessName, // direct→专用 http 工具；pi/claude/codex→curl 版 TOOLS 段
        });
        let result;
        if (this.harnessName === 'echo') {
          // echo harness：脚本由测试注入（this.echoScripts[name]）
          result = await this.harness.runTurn({
            prompt, cwd: w.dir, script: (this.echoScripts?.[w.name] || this.echoScripts?.['*'] || ''),
            serverPort: this.serverPort, peer: w.name, turn: w.turn,
          });
        } else {
          // 流式实时记账（DeepSeek 客户端式速度）：harness 边跑边回调 usage 增量，
          // live 入账让 /status 的 tokenUsage 连续增长——前端 2s 差分出的 tok/s 才是真实速度。
          // live 计数存 w 上，turn 结束时 _accumulateUsage 用权威值减去它补差（不重不漏）。
          const live = { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0 };
          result = await this.harness.runTurn({
            prompt, cwd: w.dir, model: this.model, timeoutMs: this.turnTimeoutMs, signal: this._abort.signal,
            serverPort: this.serverPort, peer: w.name, apiBase: this.apiBase, apiKey: this.apiKey,
            onUsage: (d) => {
              live.apiCalls += d.apiCalls || 0;
              live.inputTokens += d.inputTokens || 0;
              live.outputTokens += d.outputTokens || 0;
              live.cacheCreationTokens += d.cacheCreationTokens || 0;
              live.cacheReadTokens += d.cacheReadTokens || 0;
              this._accumulateUsage(w, d, 0, 0, true);
            },
          });
          // token 记账：回合权威 usage 减去 live 已入账的量，只补差——live 与最终同源时差为 0。
          const liveU = (live.apiCalls || live.inputTokens || live.outputTokens || live.cacheCreationTokens || live.cacheReadTokens)
            ? live : null;
          const finalUsage = result.usage
            ? (liveU
              ? {
                apiCalls: (result.usage.apiCalls || 0) - liveU.apiCalls,
                inputTokens: (result.usage.inputTokens || 0) - liveU.inputTokens,
                outputTokens: (result.usage.outputTokens || 0) - liveU.outputTokens,
                cacheCreationTokens: (result.usage.cacheCreationTokens || 0) - liveU.cacheCreationTokens,
                cacheReadTokens: (result.usage.cacheReadTokens || 0) - liveU.cacheReadTokens,
                turns: result.usage.turns || 0, steps: result.usage.steps || 0,
              }
              : result.usage)
            : null;
          this._accumulateUsage(w, finalUsage, prompt.length, result.steps);
        }
        // echo harness 也可能写文件后需要提交——统一在 turn 结束后检查 worktree 是否有未提交改动。
        // 返回新 HEAD（仅在确有新提交时），用于日志；无改动则不读 HEAD，省掉每回合一次 git 往返。
        const sha = await this._autoCommit(w);
        w.lastReply = (result.reply || '').slice(0, 200); // 供下回合简报（LAST TURN 段）
        if (sha && sha !== w.lastIntegratedHead) {
          w.lastIntegratedHead = sha;
          this.log('worker.new_commit', { name: w.name, sha });
        }
        const idle = /(^|\W)IDLE(\W|$)/.test(result.reply || '');
        // 记录 itemHold 开始时间（供 P0 慢任务检测）
        if (!idle && w.itemHold) w.itemHold.since = w.itemHold.since || Date.now();
        if (idle) {
          w.idleCount += 1;
          w.status = 'idle';
          this.log('worker.idle', { name: w.name, count: w.idleCount });
          // 只有 board 真的没活了才退场。否则退避等待——退场会让 autoscaler 立刻重建 worktree
          // （建 worktree + 分支是重操作），而"等待"几乎免费。这是 100-agent 压测暴露的头号 churn 源。
          let open = 0;
          try { open = this.store.openCount(); } catch { /* store 已关闭 */ }
          // 同 idle_skip：保留下限守卫，防止 retire→reap→respawn 死循环（同上）。
          if (open === 0 && w.idleCount >= this.idleTurnsToExit && this._activeCount() > this.minWorkers) {
            w.status = 'retired';
            this.log('worker.retired_idle', { name: w.name });
            break;
          }
          await sleep(this.idleBackoffMs || 5000);
        } else {
          w.idleCount = 0;
          this.log('worker.turn_done', { name: w.name, turn: w.turn, steps: result.steps, promptChars: w.promptChars || 0, usage: result.usage || null, reply: (result.reply || '').slice(0, 300) });
          // 单任务死磕熔断：连续持有同一 item 超过 maxItemTurns 回合，或该 item 累计
          // 消耗超过 maxItemTokens → 自动拆解成子任务 + 释放租约。
          // 超复杂单文件是单 worker 能力天花板，死磕不如拆小让多人分头做。
          try {
            const claims = this.store.activeClaimsByPeer(w.name);
            for (const c of claims) {
              w.itemHold = w.itemHold || {};
              if (w.itemHold.itemId === c.item_id) w.itemHold.turns += 1;
              else w.itemHold = { itemId: c.item_id, turns: 1, since: Date.now() };
              // token 熔断：item 累计 input+output 超阈值 → 放弃
              const itemUsage = this.itemTokens.get(c.item_id);
              const itemTotal = itemUsage ? (itemUsage.inputTokens + itemUsage.outputTokens) : 0;
              const tokenCap = Number(this.maxItemTokens || 2000000); // 200 万 tokens
              if (w.itemHold.turns > this.maxItemTurns || itemTotal > tokenCap) {
                this.log('worker.item_abandoned', { name: w.name, itemId: c.item_id, turns: w.itemHold.turns, itemTokens: itemTotal });
                // 自动拆解：把做不完的 item 拆成"核心实现"和"测试"两个子任务。
                // 下一个认领者看到的是更小的子任务而非原巨型任务。
                // 发散护栏：拆解速度 > 完成速度时 board 指数膨胀（压测实测拆 1.3/min vs 完 0.4/min），
                // 所以加两道闸——总 item 数上限（maxItems，默认 200）+ 拆解树深度上限（4 层）。
                // 超过任一上限就只熔断释放租约、不再拆：让别的 worker 换个思路重试原任务，
                // 而不是继续把 board 越撑越大。
                const item = this.store.item(c.item_id);
                if (item) {
                  const MAX_ITEMS = Number(this.maxItems || 200);
                  const MAX_SPLIT_DEPTH = 4;
                  const total = this.store.itemCount();
                  const depth = this.store.itemDepth(c.item_id);
                  if (total >= MAX_ITEMS || depth >= MAX_SPLIT_DEPTH) {
                    this.log('worker.item_split_capped', { name: w.name, itemId: c.item_id, total, depth, maxItems: MAX_ITEMS, maxDepth: MAX_SPLIT_DEPTH });
                  } else {
                    // 去掉扩展名避免 dedupKey 把子项折叠回原项（f: 键相同）
                    const base = item.title.replace(/\.(js|ts|py|md|go|rs|java|cpp|c|h|tsx|jsx)$/i, '');
                    try {
                      // 先标记原 item 为 split，防止 addItem 时 dedupKey 命中原项
                      this.store.db.prepare("UPDATE work_items SET status='split' WHERE id=?").run(c.item_id);
                      this.store.addItem({ title: `${base} — core implementation`, detail: `Split from "${item.title}" — core implementation only, no tests. ${item.detail || ''}`, parentId: c.item_id, createdBy: 'auto-split' });
                      this.store.addItem({ title: `${base} — tests`, detail: `Split from "${item.title}" — tests only, assumes core impl exists. ${item.detail || ''}`, parentId: c.item_id, createdBy: 'auto-split' });
                      this.log('worker.item_auto_split', { name: w.name, itemId: c.item_id, into: [`${base} — core implementation`, `${base} — tests`] });
                    } catch (e) {
                      // addItem 失败时回滚状态，让 worker 重试或继续
                      this.store.db.prepare("UPDATE work_items SET status='pending' WHERE id=?").run(c.item_id);
                      this.log('worker.item_auto_split_failed', { name: w.name, itemId: c.item_id, error: e.message });
                    }
                  }
                }
                this.store.release(c.token);
                w.itemHold = null;
              }
            }
          } catch { /* store 已关闭 */ }
        }
        if (!result.ok) {
          this.log('worker.turn_error', { name: w.name, error: result.error, rateLimited: !!result.rateLimited });
          if (result.rateLimited) {
            // 限流（第一性原理）：这是"外部过载"，不是 worker 的错，不该消耗 turn 预算，
            // 也不该让 N 个 worker 锁步（3s 一起重打只会持续压死已过载的中转站）。
            // 退避 = 5s * 2^连续限流次数，上限 60s，再叠 0~3s 抖动打散各 worker。
            w.rateLimitedStreak = (w.rateLimitedStreak || 0) + 1;
            const backoff = Math.min(60000, 5000 * Math.pow(2, Math.min(w.rateLimitedStreak - 1, 4)));
            const waitMs = backoff + Math.floor(Math.random() * 3000);
            this.log('worker.rate_limited_backoff', { name: w.name, streak: w.rateLimitedStreak, waitMs });
            w.turn -= 1; // 限流回合不计入 turn 预算
            await sleep(waitMs);
          } else {
            w.rateLimitedStreak = 0;
            await sleep(3000); // 出错退避
          }
        } else {
          w.rateLimitedStreak = 0;
        }
      }
    } finally {
      // 无论正常退出还是抛异常（崩溃），都要：标记循环已结束 + 释放残留租约，避免死锁别人。
      // loopDone 供 reaper 判断"这个 worker 的 worktree 已经没人用了"。
      w.loopDone = true;
      this._releaseWorkerClaims(w);
    }
  }
  _releaseWorkerClaims(w) {
    try {
      // release() 已含"退回 pending"对称逻辑，不用再单独处理 in_progress 残留
      for (const c of this.store.activeClaimsByPeer(w.name)) this.store.release(c.token);
    } catch { /* store 已关闭，忽略 */ }
  }
  // board 上是否还有未完成工作（pending/in_progress/submitted）。用于决定 worker 是否该退场。
  // 计入 submitted 很关键：集成器跑门禁期间工作项仍是 submitted，若不算"活"会让 worker 提前退场。
  _noOpenWork() {
    try { return this.store.openCount() === 0; } catch { return true; }
  }
  // 池内活跃 worker 数（未退场、未崩溃）。供空转退场的保留下限守卫使用。
  _activeCount() {
    let n = 0;
    for (const w of this.workers.values()) if (w.status !== 'retired' && w.status !== 'crashed') n += 1;
    return n;
  }

  // token 记账：回合级 usage 累加进 runner 全局 + 该 worker。promptChars 用于验证"简报真的进了 prompt"。
  // steps 单独记账：墙钟时间被服务端延迟波动主导（同一任务实测 99s~313s），不可作优化度量；
  // "每 turn 消耗多少 step"才是稳定可比的——step 数 × 服务端往返延迟 ≈ 推理侧总耗时。
  // live=true 是流式增量路径（onUsage 回调）：只累计 token 计数器，turns/steps 仍由
  // 回合结束时的权威调用记账（补差后的 finalUsage 已减去 live 部分，两路相加不重不漏）。
  _accumulateUsage(w, usage, promptChars = 0, steps = 0, live = false) {
    if (!live) w.promptChars = promptChars;
    if (!live) this.tokenUsage.steps = (this.tokenUsage.steps || 0) + (steps || 0);
    if (!usage) return;
    const tk = (t) => {
      t.apiCalls += usage.apiCalls || 0;
      t.inputTokens += usage.inputTokens || 0;
      t.outputTokens += usage.outputTokens || 0;
      t.cacheCreationTokens += usage.cacheCreationTokens || 0;
      t.cacheReadTokens += usage.cacheReadTokens || 0;
    };
    tk(this.tokenUsage);
    w.tokenUsage = w.tokenUsage || { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0, turns: 0 };
    tk(w.tokenUsage);
    if (!live) {
      this.tokenUsage.turns += 1;
      w.tokenUsage.turns += 1;
    }
    // 按工作项归因：worker 持有活跃租约时，把本回合 usage 记到租约指向的 item 上。
    // 一回合最多持一两条租约（协议约束），多租约时均摊（近似，够监控用）。
    // live 增量也走这里（worker 正在租约上干活，token 本来就花在这个 item 上），
    // item 的 turns 只在权威调用时 +1，避免一个 turn 被记成几十次。
    try {
      const claims = this.store.activeClaimsByPeer(w.name);
      if (claims.length > 0) {
        const per = claims.length;
        for (const c of claims) {
          const t = this.itemTokens.get(c.item_id) || { apiCalls: 0, inputTokens: 0, outputTokens: 0, turns: 0 };
          t.apiCalls += (usage.apiCalls || 0) / per;
          t.inputTokens += (usage.inputTokens || 0) / per;
          t.outputTokens += (usage.outputTokens || 0) / per;
          if (!live) t.turns += 1 / per;
          this.itemTokens.set(c.item_id, t);
        }
      }
    } catch { /* store 已关闭，跳过归因 */ }
  }

  async _autoCommit(w) {
    try {
      const status = await gitAsync(['status', '--porcelain'], w.dir);
      if (status && status.length > 0) {
        await gitAsync(['add', '-A'], w.dir);
        await gitAsync(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local',
          'commit', '-m', `wip: ${w.name} turn ${w.turn} (auto)`], w.dir);
        // 只有真的产生了新提交才读 HEAD（第一性原理：省掉每回合一次 rev-parse ≈100ms）。
        // 该 sha 仅用于日志；无条件每回合查一次 git 是纯浪费。
        return await gitAsync(['rev-parse', 'HEAD'], w.dir);
      }
    } catch { /* 无 git 或无改动，忽略 */ }
    return null;
  }

  // 回合简报：拼装 prompt 尾部的增量段。控制长度——简报越长，越接近"每回合重拉全量"，
  // 省 token 的意义就没了。所以 pending 只列前 20 个标题，context 只留 12 条。
  _buildBrief(w, d) {
    const lines = [];
    if (w.lastReply) lines.push(`LAST TURN: ${w.lastReply.slice(0, 200)}`);
    if (d.claims.length > 0) {
      lines.push(`YOUR ACTIVE CLAIMS (auto-renewed by runner): ${d.claims.map((c) => `${c.itemId} (${c.title || '?'})`).join('; ')}`);
    }
    if (d.newContext.length > 0) {
      lines.push(`NEW FINDINGS since your last turn (from other workers):`);
      for (const e of d.newContext) lines.push(`- [${e.kind}] ${e.peer}: ${String(e.text).slice(0, 160)}${e.item_id ? ` (item ${e.item_id})` : ''}`);
    }
    if (d.messages.length > 0) {
      lines.push(`MESSAGES for you:`);
      for (const m of d.messages) lines.push(`- from ${m.from_peer}${m.item_id ? ` (item ${m.item_id})` : ''}: ${String(m.text).slice(0, 200)}`);
    }
    lines.push(`BOARD SNAPSHOT: ${d.stats.pending} pending / ${d.stats.inProgress} in_progress / ${d.stats.done} done. Pending (id: title, first ${Math.min(d.pendingPreview.length, 20)}):`);
    for (const i of d.pendingPreview.slice(0, 20)) lines.push(`- ${i.id}: ${String(i.title).slice(0, 80)} [p${i.priority}]`);
    if (d.pendingPreview.length > 20) lines.push(`  ... and ${d.pendingPreview.length - 20} more, use GET /digest?limit=100`);
    return lines.join('\n');
  }

  // ---------- 集成器（独立异步循环） ----------
  startIntegrator() {
    if (this.integratorRunning) return;
    this.integratorRunning = true;
    // 事件驱动：worker 提交后立刻唤醒集成器，消除"最后一次提交 → 开始门禁"的固定轮询延迟。
    // 仍保留轮询兜底（见 _waitForSubmit）：唤醒信号与入队之间若有竞态，最多退化为旧的 200ms 延迟。
    this._integratorWake = null;
    try { this.store._onSubmit = () => this._wakeIntegrator(); } catch { /* store 只读场景忽略 */ }
    this._integratorLoop().catch((e) => { this.integratorRunning = false; this.log('integrator.crashed', { error: String(e) }); });
  }
  _wakeIntegrator() {
    const w = this._integratorWake;
    if (w) { this._integratorWake = null; w(); }
  }
  // 等待"有新提交"信号，最长 ms 毫秒（兜底轮询）。有信号则立即返回。
  _waitForSubmit(ms) {
    return new Promise((resolve) => {
      const t = setTimeout(() => { this._integratorWake = null; resolve(); }, ms);
      this._integratorWake = () => { clearTimeout(t); resolve(); };
    });
  }

  async _integratorLoop() {
    while (!this._stopped) {
      // 从 store 拉待集成提交（worker 调 /submit 写入），而不是内存队列——
      // 这样崩溃重启后未集成的提交仍可恢复，且天然串行。
      let subs = [];
      try { subs = this.store.queuedSubmissions(); } catch { subs = []; }
      // 队列空：等提交信号（事件驱动），最长 integratorPollMs 兜底。
      // 门禁完成是整条关键路径的终点，所以被唤醒必须立刻开工——这里从"固定 200ms 轮询"
      // 改成"提交即唤醒 + 兜底轮询"，把最后一次提交后的空等降到接近 0。
      if (subs.length === 0) { await this._waitForSubmit(this.integratorPollMs || 200); continue; }
      // P1 批量集成：单条提交走老路径（省掉分组开销）；多条则按"改动文件集两两不相交"
      // 贪心分组，一组只跑一次结构检 + 一次 testCmd——门禁成本从 O(N×T) 降到 O(N/K×T)。
      if (subs.length === 1) {
        await this._integrateOne(subs[0]);
        continue;
      }
      const groups = await this._groupDisjoint(subs);
      for (const batch of groups) {
        if (this._stopped) break;
        if (batch.length === 1) { await this._integrateOne(batch[0]); continue; }
        await this._integrateBatch(batch);
      }
    }
  }

  // 计算一条 submission 的改动文件集：其 worker 分支相对集成分支动了哪些文件。
  // 用集成分支做基准（不是 merge-base）：文件集=「可能与别人相撞的文件」的保守上界，
  // 相交判定宁可误杀（拆小批）也不放过（同文件进同批 → merge 冲突整批回滚，更亏）。
  async _filesOfSubmission(s) {
    const branch = `swarm/w-${s.peer}`;
    // 不必 cd 到 worktree：diff 两个分支在仓库任一 checkout 里都能算。并发算，别串行冻队列。
    const out = await gitOkAsync(['diff', '--name-only', `${this.repo.integrationBranch}...${branch}`], this.repoRoot);
    if (out == null) return null; // 分支已不存在（被 reaper 清掉等）——单条路径里会如实失败
    return new Set(out.split('\n').map((f) => f.trim()).filter(Boolean));
  }

  // 贪心分组：文件集两两不相交的 submission 归一批；有人相撞就单开一批。
  // 组内再按提交顺序排（FIFO 公平）。批大小上限 maxBatchIntegrate（默认 8）：
  // 批越大一次 testCmd 覆盖越多，但失败回滚的代价也越大（整批退回逐个集成）。
  async _groupDisjoint(subs) {
    const files = await Promise.all(subs.map((s) => this._filesOfSubmission(s)));
    const cap = Math.max(1, Number(this.maxBatchIntegrate) || 8);
    const groups = [];
    let cur = [], curFiles = new Set();
    for (let i = 0; i < subs.length; i++) {
      const fs_ = files[i];
      // 算不出文件集的（分支丢了）：单独成批，交给单条路径如实报错
      if (!fs_) { if (cur.length) { groups.push(cur); cur = []; curFiles = new Set(); } groups.push([subs[i]]); continue; }
      const clash = [...fs_].some((f) => curFiles.has(f));
      if (clash || cur.length >= cap) {
        if (cur.length) groups.push(cur);
        cur = [subs[i]]; curFiles = new Set(fs_);
      } else {
        cur.push(subs[i]);
        for (const f of fs_) curFiles.add(f);
      }
    }
    if (cur.length) groups.push(cur);
    return groups;
  }

  // 批量集成一批（≥2 条，文件集不相交）：一次门禁全过 → 逐条 done；
  // 失败 → 整批退回逐个单条集成定位毒项（毒项再失败会触发 f1 熔断，损失有界）。
  async _integrateBatch(batch) {
    // 归因预检：提交 sha 已不在分支祖先链上的（提交后被吸收/重置）不进批——
    // 批内合并是空合并也会整批记 done。捡出来走单条路径如实拒绝。
    const checked = await Promise.all(batch.map(async (s) => {
      if (!s.commit_sha) return { s, stale: false };
      const anc = await isAncestor(s.commit_sha, `swarm/w-${s.peer}`, this.repoRoot);
      return { s, stale: anc === false };
    }));
    const stales = checked.filter((c) => c.stale).map((c) => c.s);
    batch = checked.filter((c) => !c.stale).map((c) => c.s);
    for (const s of stales) await this._integrateOne(s);
    if (batch.length === 0) return;
    if (batch.length === 1) { await this._integrateOne(batch[0]); return; }
    const entries = batch.map((s) => ({ branch: `swarm/w-${s.peer}`, peer: s.peer, dir: path.join(this.repoRoot, '.swarm', 'worktrees', s.peer) }));
    const integrationId = `i-${crypto.randomBytes(4).toString('hex')}`;
    this.log('integration.batch_start', { id: integrationId, count: entries.length, peers: entries.map((e) => e.peer) });
    let r;
    try {
      r = await this.repo.integrateBatch(entries, {
        testCmd: this.testCmd, integrationId,
        message: `batch-integrate ${entries.length} submissions (${entries.map((e) => e.peer).join(', ')})`,
      });
    } catch (e) {
      r = { ok: false, stage: 'exception', reason: String(e && e.message || e).slice(0, 400), failedPeers: entries.map((e) => e.peer) };
    }
    if (r.ok) {
      // 批内逐条归因校验：提交 sha 不在批 merge 结果祖先链上的 = 空合并，拒记 done。
      const ancestry = await Promise.all(batch.map(async (s) => {
        if (!s.commit_sha) return { s, anc: true };
        return { s, anc: await isAncestor(s.commit_sha, r.sha, this.repoRoot) };
      }));
      const noopRejects = ancestry.filter((a) => a.anc === false).map((a) => a.s);
      const realMerges = ancestry.filter((a) => a.anc !== false).map((a) => a.s);
      for (const s of noopRejects) {
        const reason = `no-op merge in batch: submit sha ${String(s.commit_sha).slice(0, 10)} not in merged ${r.sha.slice(0, 10)} ancestry`;
        try {
          this.store.recordIntegration(s.peer, s.item_id, s.commit_sha, this.testCmd || '', false, false);
          this.store.resolveSubmission(s.id, false, reason);
          this.store.addContext(s.peer, 'FAIL',
            `integration rejected for item ${s.item_id} (stage=no-op): ${reason}`,
            { itemId: s.item_id, tags: ['integration', 'no-op'] });
        } catch { /* store 已关闭 */ }
      }
      if (noopRejects.length) {
        this.log('integration.batch_noop_rejected', { id: integrationId, peers: noopRejects.map((s) => s.peer), mergeSha: r.sha });
      }
      batch = realMerges;
    }
    if (r.ok && batch.length > 0) {
      // 逐条落库：recordIntegration + done。sha/diffStat 是整批共同的（一次门禁）。
      const stat = (r.diffStat || '').split('\n').slice(0, 20).join('\n').slice(0, 1200);
      for (const s of batch) {
        try {
          this.store.recordIntegration(s.peer, s.item_id, r.sha, this.testCmd || '', true, true);
          this.store.resolveSubmission(s.id, true, null);
        } catch { /* store 已关闭 */ }
      }
      if (stat.trim()) {
        // 一条 PATCH_SUMMARY 概括整批（比 N 条省 context 容量；f3 治理下的预算要省着花）
        const firstPeer = batch[0].peer;
        this.store.addContext(firstPeer, 'PATCH_SUMMARY',
          `batch-merged ${batch.length} items (${batch.map((s) => s.item_id).join(', ')}) into integration @${r.sha.slice(0, 10)}${r.ungated ? ' (ungated)' : ''}:\n${stat}`,
          { itemId: batch[0].item_id, tags: ['integration', 'patch', 'batch'] });
      }
      this.log('integration.batch_ok', { id: integrationId, sha: r.sha, count: batch.length, itemIds: batch.map((s) => s.item_id) });
    } else {
      this.log('integration.batch_failed', { id: integrationId, stage: r.stage, reason: (r.reason || '').slice(0, 300), count: batch.length });
      // 整批退回：逐个走单条集成。真正毒的那条会在单条路径里失败（→ 熔断），
      // 无辜的那几条会各自单独通过门禁——只是多花了 K-1 次 testCmd，正确性不损失。
      for (const s of batch) { await this._integrateOne(s); }
    }
  }

  // 单条集成（原 _integratorLoop 主体）：既是 1 条提交的快路径，也是批量失败后的回退路径。
  async _integrateOne(s) {
    const branch = `swarm/w-${s.peer}`;
    const dir = path.join(this.repoRoot, '.swarm', 'worktrees', s.peer);
    const integrationId = `i-${crypto.randomBytes(4).toString('hex')}`;
    this.log('integration.start', { id: integrationId, peer: s.peer, branch });
    // 归因校验：提交时记录的 commit_sha 必须仍是分支 tip 的祖先。worker 提交后分支
    // 只应向前走（auto-commit / sync merge 都是加提交，旧 tip 恒为祖先）；若不是 →
    // 分支被 P5 吸收 reset 拉回过（向后移），此刻合并它是空合并——绝不能记 done，
    // 否则 item 显示完成但代码根本没进集成分支（tui-demo 的 run_benchmark.py 事故）。
    if (s.commit_sha) {
      const anc = await isAncestor(s.commit_sha, branch, this.repoRoot);
      if (anc === false) {
        const reason = `stale submission: branch ${branch} no longer contains submit sha ${String(s.commit_sha).slice(0, 10)} (absorbed/reset since submit)`;
        this.store.recordIntegration(s.peer, s.item_id, s.commit_sha, this.testCmd || '', false, false);
        this.store.resolveSubmission(s.id, false, reason);
        this.store.addContext(s.peer, 'FAIL',
          `integration rejected for item ${s.item_id} (stage=stale): ${reason}`,
          { itemId: s.item_id, tags: ['integration', 'stale'] });
        this.log('integration.stale', { id: integrationId, peer: s.peer, itemId: s.item_id, sha: s.commit_sha });
        return;
      }
    }
    let r;
    try {
      r = await this.repo.integrate(
        { dir, branch },
        { testCmd: this.testCmd, integrationId, message: `integrate ${s.peer} (${s.item_id})` }
      );
    } catch (e) {
      r = { ok: false, stage: 'exception', reason: String(e && e.message || e).slice(0, 400) };
    }
    // 合并落地后二次校验：merge 与校验之间若发生吸收，r.sha 可能是空合并 commit。
    // 提交 sha 不在 merge commit 祖先链上 → 空合并，拒记 done。
    if (r.ok && s.commit_sha) {
      const anc = await isAncestor(s.commit_sha, r.sha, this.repoRoot);
      if (anc === false) {
        const reason = `no-op merge: submit sha ${String(s.commit_sha).slice(0, 10)} not in merged ${r.sha.slice(0, 10)} ancestry`;
        this.store.recordIntegration(s.peer, s.item_id, s.commit_sha, this.testCmd || '', false, false);
        this.store.resolveSubmission(s.id, false, reason);
        this.store.addContext(s.peer, 'FAIL',
          `integration rejected for item ${s.item_id} (stage=no-op): ${reason}`,
          { itemId: s.item_id, tags: ['integration', 'no-op'] });
        this.log('integration.noop_rejected', { id: integrationId, peer: s.peer, itemId: s.item_id, sha: s.commit_sha, mergeSha: r.sha });
        return;
      }
    }
    if (r.ok) {
      this.store.recordIntegration(s.peer, s.item_id, r.sha, this.testCmd || '', true, true);
      this.store.resolveSubmission(s.id, true, null); // 门禁过 → item done
      // P6：记录 worker 最后集成时间（供返工循环检测）
      const w = this.workers.get(s.peer);
      if (w) w.lastIntegrationTs = Date.now();
      // m5'：广播改动摘要。Agensh 的 PATCH_SUMMARY 机制——其他 worker 从简报/搜索里
      // 直接看到"集成分支上多了什么"，不必自行 diff 也能避开别人刚动过的文件。
      // diffStat 已按 1200 字符截断：够看清动了哪些文件，又不至于刷屏 context 表。
      const stat = (r.diffStat || '').split('\n').slice(0, 20).join('\n').slice(0, 1200);
      if (stat.trim()) {
        this.store.addContext(s.peer, 'PATCH_SUMMARY',
          `merged item ${s.item_id} into integration @${r.sha.slice(0, 10)}${r.ungated ? ' (ungated)' : ''}:\n${stat}`,
          { itemId: s.item_id, tags: ['integration', 'patch'] });
      }
      this.log('integration.ok', { id: integrationId, sha: r.sha, itemId: s.item_id });
      this._emit('integration.ok', { sha: r.sha, itemId: s.item_id, ungated: !!r.ungated });
    } else {
      this.store.recordIntegration(s.peer, s.item_id, s.commit_sha, this.testCmd || '', false, false);
      this.store.resolveSubmission(s.id, false, (r.reason || r.stage || '').slice(0, 400)); // 退回 pending
      // 发布 FAIL 上下文，让下一个认领者看到失败原因（m1'：带回滚点 sha + 800 字符失败输出，
      // 认领者可用 sha 对比集成分支现状；800 是 test 输出里含堆栈/断言的现实最小可读量）
      this.store.addContext(s.peer, 'FAIL',
        `integration failed for item ${s.item_id} (stage=${r.stage}, baseline=${(r.beforeSha || '').slice(0, 10) || 'unknown'}): ${(r.reason || '').slice(0, 800)}`,
        { itemId: s.item_id, tags: ['integration', r.stage || 'unknown'] });
      this.log('integration.failed', { id: integrationId, stage: r.stage, reason: (r.reason || '').slice(0, 300) });
      this._emit('integration.failed', { itemId: s.item_id, stage: r.stage, reason: (r.reason || '').slice(0, 300) });
    }
  }

  // ---------- 租约自动续期（独立循环） ----------
  // worker 的 turn 可能长达十几分钟，远超声明的 leaseSec；不能指望模型每回合记得调 /renew。
  // 这里独立续期所有活跃租约，间隔取 leaseSec 的 1/3，确保任何时刻都有 2/3 的余量。
  // 只对"仍有对应存活 worker"的租约续期，worker 退场后交给 _releaseWorkerClaims 释放。
  startLeaseRenewer() {
    if (this._renewerRunning) return;
    this._renewerRunning = true;
    this._renewerLoop().catch((e) => { this._renewerRunning = false; this.log('renewer.crashed', { error: String(e) }); });
  }
  async _renewerLoop() {
    const interval = Math.max(1000, Math.floor(this.leaseSec / 3) * 1000);
    while (!this._stopped) {
      await sleep(interval);
      try {
        for (const c of this.store.activeClaims()) {
          const w = this.workers.get(c.peer);
          if (w && w.status !== 'retired' && w.status !== 'crashed') this.store.renew(c.token, this.leaseSec);
        }
      } catch { /* store 已关闭，忽略 */ }
    }
  }

  // ---------- autoscaler（独立异步循环，只看计数不看内容） ----------
  startAutoscaler() {
    this._autoscalerLoop().catch((e) => this.log('autoscaler.crashed', { error: String(e) }));
  }
  async _autoscalerLoop() {
    // 扩容限速：worktree+进程是重操作，别一次拉上百个。但每轮上限不能太小——
    // 旧值 8 × 5s/轮意味着扩到 1000 worker 要 ~625s，远超"墙钟时间最短"的目标。
    const maxSpawnPerRound = Number(this.maxSpawnPerRound || 64);
    const intervalMs = Number(this.autoscalerIntervalMs || 1000);
    const spawnConcurrency = Number(this.spawnConcurrency || 16); // git worktree add 的并发上限，防文件系统抖动
    while (!this._stopped) {
      await sleep(intervalMs);
      // 广度信号：openCount = 未完成工作项数（含被认领的）。
      // 深度信号：recentSplitCount = 最近拆解出的子项数——单一大任务时靠它扩容，
      //          因为大 item 的 pending 只有 1~2 个，光看 pending 永远停在 minWorkers。
      let open = 0, recentSplit = 0;
      try { open = this.store.openCount(); recentSplit = this.store.recentSplitCount(); } catch { /* store 已关闭 */ }
      let active = [...this.workers.values()].filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
      // 每一项未完成工作都可能需要一个 worker；近期拆解说明并行度正在展开，提前加人。
      const demand = Math.max(open, recentSplit);
      // 任务比例护栏：并行度跟随真实可认领任务数，别让多余 worker 空转抢同一批任务
      // （空转 + 重复子项 + 限流）。上限 = max(8, open*2)，下限仍由 minWorkers 保证。
      const capped = Math.min(demand, Math.max(8, open * 2));
      let want = Math.min(this.maxWorkers, Math.max(this.minWorkers, capped));
      // 系统资源护栏：机器喘不过气时把 want 钳到护栏上限（high=只许停不许扩，
      // critical=主动缩一个）。与任务比例护栏串联——两层取最小，谁紧听谁的。
      // 事件只在压力档位变化时记（每秒一轮的循环不能刷屏）。
      if (this.resourceGuard) {
        const sample = this.sysProbe ? this.sysProbe() : sampleSystem();
        const g = guardCeiling(sample, { active });
        const prev = this._guardState.pressure;
        this._guardState = { pressure: g.pressure, ceiling: g.ceiling, sample };
        if (g.pressure !== prev) {
          this.log('autoscaler.guard_pressure', { pressure: g.pressure, ceiling: g.ceiling, cpuBusy: sample.cpuBusy, freeMemRatio: +sample.freeMemRatio.toFixed(3), active, want });
          if (g.pressure !== 'ok') this._emit('autoscaler.guard_pressure', { pressure: g.pressure, ceiling: g.ceiling, active });
        }
        // ceiling=null 表示机器健康、不设上限（用户 maxWorkers 说了算）。
        if (g.ceiling != null) want = Math.min(want, g.ceiling);
      }      if (want > active) {
        const need = Math.min(want - active, maxSpawnPerRound); // 限速
        // 并发 spawn：spawnWorker 内部首行同步生成名字，故循环调用不会撞名；
        // addWorktree 是异步子进程，串行 await 会让扩容时间随数量线性增长。
        for (let i = 0; i < need; i += spawnConcurrency) {
          const batch = [];
          for (let j = i; j < Math.min(i + spawnConcurrency, need); j++) batch.push(this.spawnWorker());
          await Promise.all(batch);
          if (this._stopped) break;
        }
      } else if (want < active) {
        // 缩容：只标记 idle 的 retire，让 worker 循环自然退出（不 kill 强杀）
        for (const w of this.workers.values()) {
          if (active <= want) break;
          if (w.status === 'idle') { w.status = 'retired'; active -= 1; }
        }
      }
      // P0 动态负载均衡：检测慢任务并强制拆分
      await this._detectAndSplitSlowItems();
      // P6 返工循环检测：高 token 输出但无集成的 worker 强制提交
      await this._detectReworkLoops();
    }
  }

  // P0 动态负载均衡：检测持有同一 item 超过 slowItemMs 且 token 速率低的 worker，强制拆分
  async _detectAndSplitSlowItems() {
    // echo harness 是离线测试，不走真实 LLM 速率分析，跳过检测
    if (this.harnessName === 'echo') return;
    try {
      const now = Date.now();
      const avgRate = this._avgTokenRate();
      for (const w of this.workers.values()) {
        if (w.status !== 'working' || !w.itemHold) continue;
        const holdMs = now - (w.itemHold.since || 0);
        if (holdMs < this.slowItemMs) continue;
        // 计算该 worker 在此 item 上的 token 速率
        const itemUsage = this.itemTokens.get(w.itemHold.itemId);
        const itemTokens = itemUsage ? (itemUsage.inputTokens + itemUsage.outputTokens) : 0;
        const rate = itemTokens / (holdMs / 1000); // tokens per second
        // 绝对兜底：持有超过 10 分钟即拆，不看相对速率（防全员高速时无人触发）
        const isSlow = holdMs > 600000 || rate < avgRate * 0.5;
        if (isSlow) {
          this.log('worker.slow_item_detected', { name: w.name, itemId: w.itemHold.itemId, holdMs, rate, avgRate });
          // 强制拆分：复用现有的 auto-split 逻辑
          const claims = this.store.activeClaimsByPeer(w.name);
          for (const c of claims) {
            if (c.item_id !== w.itemHold.itemId) continue;
            const item = this.store.item(c.item_id);
            if (!item) continue;
            const MAX_ITEMS = Number(this.maxItems || 200);
            const MAX_SPLIT_DEPTH = 4;
            const total = this.store.itemCount();
            const depth = this.store.itemDepth(c.item_id);
            if (total < MAX_ITEMS && depth < MAX_SPLIT_DEPTH) {
              // 去掉扩展名避免 dedupKey 折叠（f: 键相同会导致子项被吞）
              const base = item.title.replace(/\.(js|ts|py|md|go|rs|java|cpp|c|h|tsx|jsx)$/i, '');
              try {
                // 先标记原 item 为 split，防止 addItem 时 dedupKey 命中原项
                this.store.db.prepare("UPDATE work_items SET status='split' WHERE id=?").run(c.item_id);
                this.store.addItem({ title: `${base} — part A`, detail: `Split from "${item.title}" — slow task auto-split part A. ${item.detail || ''}`, parentId: c.item_id, createdBy: 'auto-split-slow' });
                this.store.addItem({ title: `${base} — part B`, detail: `Split from "${item.title}" — slow task auto-split part B. ${item.detail || ''}`, parentId: c.item_id, createdBy: 'auto-split-slow' });
                this.log('worker.slow_item_split', { name: w.name, itemId: c.item_id, into: [`${base} — part A`, `${base} — part B`] });
              } catch (e) {
                // addItem 失败时回滚状态，让 worker 重试或继续
                this.store.db.prepare("UPDATE work_items SET status='pending' WHERE id=?").run(c.item_id);
                this.log('worker.slow_item_split_failed', { name: w.name, itemId: c.item_id, error: e.message });
              }
            }
            this.store.release(c.token);
            w.itemHold = null;
            break;
          }
        }
      }
    } catch { /* store 已关闭 */ }
  }

  // P6 返工循环检测：worker 持续高 token 输出但长时间无集成 → 强制提交当前进度
  async _detectReworkLoops() {
    // echo harness 是离线测试，不走真实集成，跳过检测
    if (this.harnessName === 'echo') return;
    try {
      const now = Date.now();
      // 冷却：同一 worker 命中后 60s 内不再重报/重提交。
      // 没有它 autoscaler 每秒一轮会把同一 worker 刷出几百条事件 + 每秒 git add/commit 抖动。
      this._reworkCooldown = this._reworkCooldown || new Map(); // name → ts
      for (const w of this.workers.values()) {
        if (w.status !== 'working') continue;
        const lastFired = this._reworkCooldown.get(w.name) || 0;
        if (now - lastFired < 60000) continue;
        // 距上次集成的时长：从未集成过的 worker 从启动时刻起算
        const lastIntegration = now - (w.lastIntegrationTs || this._startedAt);
        if (lastIntegration < this.reworkThresholdMs) continue;
        // 检查该 worker 的 token 输出
        const usage = w.tokenUsage;
        if (!usage || usage.outputTokens < 50000) continue; // 至少 5 万 output tokens
        // 高输出 + 长时间无集成 = 返工循环
        this.log('worker.rework_loop_detected', { name: w.name, outputTokens: usage.outputTokens, lastIntegrationMs: lastIntegration });
        // 强制提交：触发一次 _autoCommit（不等待回合结束）
        // 注意：_autoCommit 只是本地 commit，不等于集成成功——不重置 lastIntegrationTs，
        // 让 integrator 真正落地时（_integrateOne 成功路径）再重置。否则会掩盖真实返工。
        const sha = await this._autoCommit(w);
        if (sha) {
          this.log('worker.rework_forced_commit', { name: w.name, sha });
          // 自动 submit：worker 已陷返工循环，多半不会主动 finish/submit。
          // 把当前进度推到集成队列让门禁裁决——过则落地，不过则打回 pending 让别人重试，
          // 都比无限 rework 强。无 active claim 时 submitWork 返回 null，无害。
          try {
            const claims = this.store.activeClaimsByPeer(w.name);
            for (const c of claims) {
              const r = this.store.submitWork(c.token, w.name, sha);
              if (r) {
                this.log('worker.rework_auto_submitted', { name: w.name, itemId: r.itemId, submissionId: r.submissionId });
                w.itemHold = null;
              }
            }
          } catch { /* store 已关闭 */ }
        }
        // 无论本次是否真的产生新提交，都记冷却——防止"worker 在写文件 → status 为空 →
        // 下一秒又命中"的刷屏循环。冷却到点再检测，给 integrator 60s 落地窗口。
        this._reworkCooldown.set(w.name, now);
      }
    } catch { /* store 已关闭 */ }
  }

  // 计算全局平均 token 速率（output tokens / 秒）
  _avgTokenRate() {
    const elapsed = (Date.now() - this._startedAt) / 1000;
    if (elapsed <= 0) return 1000; // 默认 1000 tok/s
    return (this.tokenUsage.outputTokens || 0) / elapsed;
  }

  // ---------- reaper（独立循环）：回收退场 worker 的 worktree，防止长跑磁盘泄漏 ----------
  // worker 循环结束后，其 worktree + 分支仍留在磁盘上。worker 池频繁伸缩（autoscaler 每 5s 一轮）
  // 时会累积成百上千个 worktree，必须回收。但回收有两个前提，否则会破坏正在进行的集成：
  //   1. 循环已结束（loopDone）——不能回收一个还在跑的 worker 的 worktree；
  //   2. 没有未集成的提交（hasQueuedSubmissionForPeer）——集成器 merge 需要该分支存在，
  //      提前删分支会让集成失败。等集成器把该提交 resolve 掉，下一轮再回收。
  startReaper() {
    if (this._reaperRunning) return;
    this._reaperRunning = true;
    this._reaperLoop().catch((e) => { this._reaperRunning = false; this.log('reaper.crashed', { error: String(e) }); });
  }
  async _reaperLoop() {
    const interval = Number(this.reaperIntervalMs || 10000);
    while (!this._stopped) {
      await sleep(interval);
      let reaped = 0;
      for (const w of [...this.workers.values()]) {
        if (!w.loopDone) continue;
        if (w.status !== 'retired' && w.status !== 'crashed') continue;
        try { if (this.store.hasQueuedSubmissionForPeer(w.name)) continue; } catch { continue; }
        // 失败退避：上次清理失败后间隔翻倍重试（上限 5min），避免每 10s 刷屏。
        // Windows 上目录句柄释放有延迟，立即重试几乎必败。
        if (w.reapFailAt && Date.now() - w.reapFailAt < Math.min(300000, interval * Math.pow(2, w.reapFailCount || 0))) continue;
        const r = await this.repo.removeWorktree(w.name);
        if (r && r.ok === false) {
          w.reapFailCount = (w.reapFailCount || 0) + 1;
          w.reapFailAt = Date.now();
          this.log('worker.reap_failed', { name: w.name, attempts: w.reapFailCount });
          continue;
        }
        this.workers.delete(w.name); // 从池里移除，让 autoscaler 的 active 计数正确
        reaped += 1;
        this.log('worker.reaped', { name: w.name, status: w.status });
      }
      if (reaped) this.log('reaper.swept', { reaped, remaining: this.workers.size });
    }
  }

  // ---------- 生命周期 ----------
  async stop() {
    this._stopped = true;
    this._abort.abort(); // 立刻杀掉正在跑的 harness 子进程，否则 drain 会等到 turnTimeoutMs
    await this.drain();
    const failed = [];
    for (const w of this.workers.values()) {
      const r = await this.repo.removeWorktree(w.name);
      if (r && r.ok === false) failed.push(w.name);
    }
    if (failed.length) this.log('stop.cleanup_failed', { names: failed.join(',') });
    return failed;
  }
  async drain() { // 等所有 worker 循环真正结束（避免 store 关闭后仍写日志）
    await Promise.all([...this.workers.values()].map((w) => w.loop).filter(Boolean));
  }
  // 预算口径：input+output+cacheCreation+cacheRead 之和（apiCalls/turns/steps 不算 token）。
  _tokensSpent() {
    return (this.tokenUsage.inputTokens || 0) + (this.tokenUsage.outputTokens || 0)
      + (this.tokenUsage.cacheCreationTokens || 0) + (this.tokenUsage.cacheReadTokens || 0);
  }

  // 供监控面板（GET /status）调用的只读快照。每 2s 被轮询一次，故各查询都要走索引/LIMIT。
  status() {
    const workers = [...this.workers.values()].map((w) => ({ name: w.name, status: w.status, turn: w.turn, idle: w.idleCount, syncSkew: w.syncSkew || 0 }));
    const active = workers.filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
    this._peakActive = Math.max(this._peakActive || 0, active);
    // crash 分类统计：从事件流里聚合（不靠 worker 实例状态——已 crash 的 worker 会被 reaper 清掉）
    const crashStats = {};
    try {
      for (const e of this.store.eventsByType('worker.crashed', 500)) {
        const cat = (e.payload && e.payload.category) || 'unknown';
        crashStats[cat] = (crashStats[cat] || 0) + 1;
      }
    } catch { /* store 已关闭 */ }
    let integrations = [];
    let stats = {};
    try {
      integrations = this.store.recentIntegrations(20);
      stats = this.store.stats();
    } catch { /* store 已关闭 */ }
    return {
      workers,
      active,
      peakActive: this._peakActive || active,
      minWorkers: this.minWorkers,
      maxWorkers: this.maxWorkers,
      testCmd: this.testCmd || null,
      startedAt: this._startedAt || null,
      briefEnabled: this.briefEnabled,
      budget: { tokens: this.budgetTokens, spent: this._tokensSpent(), exceeded: !!this.budgetExceeded },
      // 资源护栏实时状态：监控面板据此显示"当前被机器资源压着"而非误判 swarm 卡住。
      resourceGuard: {
        enabled: this.resourceGuard,
        pressure: this._guardState.pressure,
        ceiling: this._guardState.ceiling,
        sample: this._guardState.sample,
      },
      tokenUsage: this.tokenUsage,
      itemTokens: Object.fromEntries(this.itemTokens),
      apiInflight: apiInflight(),
      integrations,
      stats,
      crashStats,
    };
  }
}

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
