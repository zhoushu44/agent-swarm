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
import { Repo, gitAsync } from './gitutil.js';
import { getHarness, apiInflight } from './harness.js';
import { buildWorkerPrompt } from './prompts.js';

export class Runner {
  constructor({ store, repoRoot, serverPort, apiToken, harness = 'claude', model, mission, conventions, testCmd, maxTurns = 10, turnTimeoutMs = 900000, idleTurnsToExit = 2, maxWorkers = 8, minWorkers = 1, leaseSec = 300, worktreeCache = new Map(), apiBase = null, apiKey = null, briefEnabled = true }) {
    this.store = store; this.repoRoot = repoRoot; this.serverPort = serverPort; this.apiToken = apiToken;
    this.harnessName = harness; this.harness = getHarness(harness); this.model = model;
    this.apiBase = apiBase; this.apiKey = apiKey;
    this.mission = mission; this.conventions = conventions; this.testCmd = testCmd;
    this.maxTurns = maxTurns; this.turnTimeoutMs = turnTimeoutMs; this.idleTurnsToExit = idleTurnsToExit;
    this.maxWorkers = maxWorkers; this.minWorkers = minWorkers;
    this.leaseSec = leaseSec;
    // briefEnabled=false 时关闭"回合简报"（A/B 对照用）：worker 每回合自己拉全量。
    this.briefEnabled = briefEnabled !== false;
    // token 记账：所有 worker 所有回合的累计（省 token 实验的测量口）。
    this.tokenUsage = { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0, turns: 0, steps: 0 };
    // 按工作项归因的 token（key=item_id）：监控面板用差分算每项实时 tok/s。
    // 没有租约的回合（拆解/idle/重试）只进全局，不硬摊。
    this.itemTokens = new Map();
    this.repo = new Repo(repoRoot);
    this.workers = new Map(); // name → { name, dir, branch, proc, idleCount, status }
    this.integratorRunning = false;
    this._stopped = false;
    this._nameCounter = 0;
    // 中止通道：stop() 时 abort，正在跑的 harness 子进程立刻被杀（否则 drain 会卡到 turnTimeoutMs）
    this._abort = new AbortController();
    this._startedAt = Date.now();
    this._peakActive = 0;
    this.log = (type, payload) => { try { store.log('runner', type, payload); } catch { /* store 已关闭，忽略 */ } };
  }

  // ---------- worker 生命周期 ----------
  async spawnWorker(name = null) {
    if (!name) name = this._nextName();
    if (this.workers.has(name)) return this.workers.get(name);
    const wt = await this.repo.addWorktree(name);
    const w = {
      name, dir: wt.dir, branch: wt.branch, proc: null, idleCount: 0, status: 'starting', turn: 0,
      // 不再在 spawn 时同步 git rev-parse（autoscaler 一轮可能连拉 8 个 worker，
      // 8 × ~100ms 会冻住事件循环）。该值仅用于 worker.new_commit 日志，置 null 即可。
      lastIntegratedHead: null,
      lastDigestTs: 0, lastReply: '', // 回合简报用：上次 digest 时间 + 上回合摘要
    };
    this.workers.set(name, w);
    this.log('worker.spawned', { name, dir: wt.dir });
    w.loop = this._runWorkerLoop(w).catch((e) => { w.status = 'crashed'; this.log('worker.crashed', { name, error: String(e) }); });
    return w;
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
        if (synced.epoch !== undefined) w.lastSyncEpoch = synced.epoch;
        if (synced.conflict) this.log('worker.sync_conflict', { name: w.name });
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
          result = await this.harness.runTurn({ prompt, cwd: w.dir, model: this.model, timeoutMs: this.turnTimeoutMs, signal: this._abort.signal, serverPort: this.serverPort, peer: w.name, apiBase: this.apiBase, apiKey: this.apiKey });
        }
        // token 记账：把本回合的 usage 累计进 runner 与 worker，供"省 token"实验直接读取。
        this._accumulateUsage(w, result.usage, prompt.length, result.steps);
        // echo harness 也可能写文件后需要提交——统一在 turn 结束后检查 worktree 是否有未提交改动。
        // 返回新 HEAD（仅在确有新提交时），用于日志；无改动则不读 HEAD，省掉每回合一次 git 往返。
        const sha = await this._autoCommit(w);
        w.lastReply = (result.reply || '').slice(0, 200); // 供下回合简报（LAST TURN 段）
        if (sha && sha !== w.lastIntegratedHead) {
          w.lastIntegratedHead = sha;
          this.log('worker.new_commit', { name: w.name, sha });
        }
        const idle = /(^|\W)IDLE(\W|$)/.test(result.reply || '');
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
  _accumulateUsage(w, usage, promptChars = 0, steps = 0) {
    w.promptChars = promptChars;
    this.tokenUsage.steps = (this.tokenUsage.steps || 0) + (steps || 0);
    if (!usage) return;
    this.tokenUsage.turns += 1;
    this.tokenUsage.apiCalls += usage.apiCalls || 0;
    this.tokenUsage.inputTokens += usage.inputTokens || 0;
    this.tokenUsage.outputTokens += usage.outputTokens || 0;
    this.tokenUsage.cacheCreationTokens += usage.cacheCreationTokens || 0;
    this.tokenUsage.cacheReadTokens += usage.cacheReadTokens || 0;
    w.tokenUsage = w.tokenUsage || { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0, turns: 0 };
    w.tokenUsage.turns += 1;
    w.tokenUsage.apiCalls += usage.apiCalls || 0;
    w.tokenUsage.inputTokens += usage.inputTokens || 0;
    w.tokenUsage.outputTokens += usage.outputTokens || 0;
    w.tokenUsage.cacheCreationTokens += usage.cacheCreationTokens || 0;
    w.tokenUsage.cacheReadTokens += usage.cacheReadTokens || 0;
    // 按工作项归因：worker 持有活跃租约时，把本回合 usage 记到租约指向的 item 上。
    // 一回合最多持一两条租约（协议约束），多租约时均摊（近似，够监控用）。
    try {
      const claims = this.store.activeClaimsByPeer(w.name);
      if (claims.length > 0) {
        const per = claims.length;
        for (const c of claims) {
          const t = this.itemTokens.get(c.item_id) || { apiCalls: 0, inputTokens: 0, outputTokens: 0, turns: 0 };
          t.apiCalls += (usage.apiCalls || 0) / per;
          t.inputTokens += (usage.inputTokens || 0) / per;
          t.outputTokens += (usage.outputTokens || 0) / per;
          t.turns += 1 / per;
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
      const s = subs[0];
      const branch = `swarm/w-${s.peer}`;
      const dir = path.join(this.repoRoot, '.swarm', 'worktrees', s.peer);
      const integrationId = `i-${crypto.randomBytes(4).toString('hex')}`;
      this.log('integration.start', { id: integrationId, peer: s.peer, branch });
      let r;
      try {
        r = await this.repo.integrate(
          { dir, branch },
          { testCmd: this.testCmd, integrationId, message: `integrate ${s.peer} (${s.item_id})` }
        );
      } catch (e) {
        r = { ok: false, stage: 'exception', reason: String(e && e.message || e).slice(0, 400) };
      }
      if (r.ok) {
        this.store.recordIntegration(s.peer, s.item_id, r.sha, this.testCmd || '', true, true);
        this.store.resolveSubmission(s.id, true, null); // 门禁过 → item done
        this.log('integration.ok', { id: integrationId, sha: r.sha, itemId: s.item_id });
      } else {
        this.store.recordIntegration(s.peer, s.item_id, s.commit_sha, this.testCmd || '', false, false);
        this.store.resolveSubmission(s.id, false, (r.reason || r.stage || '').slice(0, 400)); // 退回 pending
        // 发布 FAIL 上下文，让下一个认领者看到失败原因
        this.store.addContext(s.peer, 'FAIL', `integration failed for item ${s.item_id} (stage=${r.stage}): ${(r.reason || '').slice(0, 300)}`,
          { itemId: s.item_id, tags: ['integration', r.stage || 'unknown'] });
        this.log('integration.failed', { id: integrationId, stage: r.stage, reason: (r.reason || '').slice(0, 300) });
      }
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
      const want = Math.min(this.maxWorkers, Math.max(this.minWorkers, demand));
      if (want > active) {
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
    }
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
        await this.repo.removeWorktree(w.name);
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
    for (const w of this.workers.values()) await this.repo.removeWorktree(w.name);
  }
  async drain() { // 等所有 worker 循环真正结束（避免 store 关闭后仍写日志）
    await Promise.all([...this.workers.values()].map((w) => w.loop).filter(Boolean));
  }
  // 供监控面板（GET /status）调用的只读快照。每 2s 被轮询一次，故各查询都要走索引/LIMIT。
  status() {
    const workers = [...this.workers.values()].map((w) => ({ name: w.name, status: w.status, turn: w.turn, idle: w.idleCount }));
    const active = workers.filter((w) => w.status !== 'retired' && w.status !== 'crashed').length;
    this._peakActive = Math.max(this._peakActive || 0, active);
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
      tokenUsage: this.tokenUsage,
      itemTokens: Object.fromEntries(this.itemTokens),
      apiInflight: apiInflight(),
      integrations,
      stats,
    };
  }
}

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
