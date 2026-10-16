// git 工具 —— worktree 隔离 + 集成分支管理。
// 关键设计（来自 codex-agensh 的教训）：
//   - 每个 worker 一个 worktree（互不干扰）
//   - 集成分支是共享真理：合并时先跑门禁测试，过才 merge
//   - 合并提交带 Agensh-Integration trailer，崩溃后可从 git log 恢复
import { exec, execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

// 同步版：只用于初始化/探测等一次性路径（init/head/recover），不进 worker 回合热路径。
export function git(args, cwd, opts = {}) {
  return execFileSync('git', args, {
    cwd, encoding: 'utf8', maxBuffer: 64e6,
    env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
    ...opts,
  }).trim();
}

export function gitOk(args, cwd) {
  try { return git(args, cwd); } catch { return null; }
}

// 异步版：worker 回合热路径必须走它。所有 worker 共享同一个 Node 事件循环，
// execFileSync 会把整个进程冻住，导致其它 worker 的 API 响应/工具调用全部排队。
export function gitAsync(args, cwd, opts = {}) {
  return new Promise((resolve, reject) => {
    execFile('git', args, {
      cwd, encoding: 'utf8', maxBuffer: 64e6, windowsHide: true,
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
      ...opts,
    }, (e, stdout, stderr) => {
      if (e) { e.message = `${stdout || ''}${stderr || ''}${e.message || ''}`; reject(e); return; }
      resolve(String(stdout || '').trim());
    });
  });
}

export async function gitOkAsync(args, cwd) {
  try { return await gitAsync(args, cwd); } catch { return null; }
}

export class Repo {
  constructor(root) {
    this.root = path.resolve(root);
    this.integrationBranch = 'swarm/integration';
    // 集成纪元：集成分支每被推进一次 +1。只有集成器会推进它，所以在单进程模型里
    // 这个内存计数器就是"集成分支是否有新进展"的权威信号——worker 每回合的基线刷新
    // 据此短路：纪元没变就完全跳过（省掉 status + merge 两次子进程往返）。
    this.integrationEpoch = 0;
    // 集成分支 HEAD 的内存镜像（同 integrationEpoch 的假设：单进程内只有集成器推进它）。
    // 有了它，integrate() 合并前无需再跑一次 `rev-parse HEAD` 拿回滚点——那是每次合并
    // 一次子进程往返。首个合并时仍读一次，之后由本字段维护。
    this.integrationHead = null;
    // 集成 worktree 是否已就绪。就绪后 ensureIntegrationWorktree() 直接返回，省掉每次合并
    // 一次的同步 fs.existsSync（虽小，但在集成器热路径上，且是同步阻塞）。
    this._itDirReady = false;
  }

  init(baseBranch = null) {
    if (!fs.existsSync(path.join(this.root, '.git'))) {
      fs.mkdirSync(this.root, { recursive: true });
      git(['init', '-b', 'main'], this.root);
    }
    // 空仓库（无任何 commit）时 rev-parse 会抛错，先补一个 initial commit
    const hasHead = !!gitOk(['rev-parse', '--verify', 'HEAD'], this.root);
    if (!hasHead) {
      const keep = path.join(this.root, '.gitignore');
      if (!fs.existsSync(keep)) fs.writeFileSync(keep, '.swarm/\n');
      git(['add', '-A'], this.root);
      git(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', 'chore: swarm initial commit'], this.root);
    }
    const branches = (gitOk(['branch', '--list', this.integrationBranch], this.root) || '').trim();
    if (!branches) {
      const base = baseBranch || git(['rev-parse', '--abbrev-ref', 'HEAD'], this.root);
      git(['branch', this.integrationBranch, base], this.root);
    }
    return this;
  }

  // 为 worker 建 worktree：从集成分支最新位置开新分支。
  // 必须异步：autoscaler 一轮可能连拉 8 个 worker，每个 worktree add 是 ~100-500ms 的子进程，
  // 同步实现会把整个事件循环冻住 8 次，期间所有 worker 的 API 往返全部排队。
  async addWorktree(name, { fresh = true } = {}) {
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    if (fs.existsSync(dir)) {
      await gitOkAsync(['worktree', 'remove', '--force', dir], this.root);
      await gitOkAsync(['branch', '-D', `swarm/w-${name}`], this.root);
    }
    await gitAsync(['worktree', 'add', '-b', `swarm/w-${name}`, dir, this.integrationBranch], this.root);
    return { name, dir, branch: `swarm/w-${name}` };
  }

  async removeWorktree(name) {
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    await gitOkAsync(['worktree', 'remove', '--force', dir], this.root);
    await gitOkAsync(['branch', '-D', `swarm/w-${name}`], this.root);
  }

  // ---- 集成分支专用 worktree ----
  // 集成分支必须有一个独立 checkout：绝不能在用户主工作区里 `checkout swarm/integration`
  // （会覆盖/丢失用户未提交改动）。集成器所有 merge、门禁测试都在这个 worktree 里做，
  // 主工作区永远停在用户自己的分支上。
  integrationDir() { return path.join(this.root, '.swarm', 'integration'); }
  ensureIntegrationWorktree() {
    const dir = this.integrationDir();
    if (this._itDirReady) return dir; // 已就绪：省掉热路径上的同步 fs.existsSync
    if (fs.existsSync(path.join(dir, '.git'))) { this._itDirReady = true; return dir; }
    if (fs.existsSync(dir)) gitOk(['worktree', 'remove', '--force', dir], this.root); // 清理残留
    git(['worktree', 'add', dir, this.integrationBranch], this.root);
    this._linkDeps(dir);
    this._itDirReady = true;
    return dir;
  }
  // 门禁测试常在 worktree 里跑，但依赖目录（node_modules）通常只存在于主工作区。
  // 尽力把它链进来（Windows 用 junction 免管理员权限），否则门禁会因缺依赖误报失败。
  _linkDeps(itDir) {
    const src = path.join(this.root, 'node_modules');
    const dst = path.join(itDir, 'node_modules');
    if (!fs.existsSync(src) || fs.existsSync(dst)) return;
    try {
      fs.symlinkSync(src, dst, process.platform === 'win32' ? 'junction' : 'dir');
    } catch { /* 链接失败不致命：门禁可能因此失败，但不会破坏主工作区 */ }
  }

  // 把 worker 分支的基线刷新到集成分支最新位置（每个 turn 开工前调用）。
  // 不这么做的话，集成分支推进几十上百次后 worker 基线极旧，merge 会大面积冲突——
  // 这是 Agensh 类架构在规模下的头号杀手。返回 { ok, conflict, skipped }。
  // sinceEpoch：调用方上次刷新时的集成纪元。与当前纪元相同 → 集成分支无新进展，
  // 直接跳过（省掉 status + merge 两次子进程往返，这是每回合的固定开销）。
  async syncWorktree(name, sinceEpoch = null) {
    if (sinceEpoch !== null && sinceEpoch === this.integrationEpoch) {
      return { ok: true, skipped: true, epoch: this.integrationEpoch };
    }
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    if (!fs.existsSync(dir)) return { ok: false, reason: 'no worktree' };
    // 工作区必须干净才能安全 merge（turn 结束已 auto-commit，正常应为干净）
    const dirty = await gitOkAsync(['status', '--porcelain'], dir);
    if (dirty) {
      await gitOkAsync(['add', '-A'], dir);
      await gitOkAsync(['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', `wip: ${name} (pre-sync)`], dir);
    }
    try {
      await gitAsync(['merge', '--no-edit', this.integrationBranch], dir);
      return { ok: true, epoch: this.integrationEpoch };
    } catch (e) {
      await gitOkAsync(['merge', '--abort'], dir); // 有冲突：放弃刷新，本回合继续用旧基线（不阻塞 worker）
      // 冲突时也记下纪元：否则每回合都会重试同一个必然冲突的 merge，白烧两次子进程往返。
      return { ok: false, conflict: true, epoch: this.integrationEpoch, reason: String(e.message).slice(0, 300) };
    }
  }

  // ---- 合并门禁：worker 分支 → 集成分支 ----
  // testCmd 返回 { passed, output }。在集成分支专用 worktree 里 merge --no-ff、跑门禁、过则保留，败则回滚。
  // 关键：全程不碰用户主工作区（不 checkout 到 root），用户随时可继续在自己的分支上工作。
  async integrate(worktree, { testCmd, integrationId, message }) {
    const { branch } = worktree;
    const itDir = this.ensureIntegrationWorktree();
    const trailer = `Agensh-Integration: ${integrationId}`;
    const msg = (message || `swarm integrate ${branch}`) + `\n\n${trailer}`;
    let beforeSha;
    try {
      // 回滚点：优先用内存镜像（单进程里只有集成器推进集成分支），省掉一次 `rev-parse HEAD`
      // 子进程往返。首次（镜像为空）或镜像与集成分支失联时，才真的读一次。
      beforeSha = this.integrationHead || (this.integrationHead = await gitAsync(['rev-parse', 'HEAD'], itDir));
      await gitAsync(['merge', '--no-ff', '-m', msg, branch], itDir);
    } catch (e) {
      await gitOkAsync(['merge', '--abort'], itDir);
      return { ok: false, stage: 'merge', reason: String(e.message).slice(0, 400) };
    }
    // 门禁测试在集成分支合并结果上跑（在集成 worktree 里，不污染主工作区）。
    // 必须异步：门禁可能跑几十秒，spawnSync 会冻住整个事件循环、卡住所有 worker 的 API 往返。
    let passed = false, output = '';
    if (testCmd) {
      try {
        const r = await runShell(testCmd, itDir);
        passed = r.code === 0;
        output = r.output.slice(0, 4000);
      } catch (e) { output = String(e.message).slice(0, 4000); passed = false; }
    } else passed = true; // 无测试命令 = 无门禁，直接过
    if (passed) {
      // 集成分支推进成功 → 纪元 +1。worker 下回合的基线刷新据此知道"有新东西要 merge"。
      this.integrationEpoch += 1;
      const sha = await gitAsync(['rev-parse', 'HEAD'], itDir);
      this.integrationHead = sha; // 维护镜像，供下一次合并免读回滚点
      return { ok: true, sha, stage: 'gated', epoch: this.integrationEpoch };
    }
    // 回滚集成：reset 回合并前的 sha（不能用 HEAD~1，no-op 合并时会误删上一个集成）
    await gitOkAsync(['reset', '--hard', beforeSha], itDir);
    return { ok: false, stage: 'test', reason: output };
  }

  // 崩溃恢复：扫 git log 里的 integration trailer，找出已合并但 store 里没记录的
  recoverIntegrations(store) {
    const logFmt = '%H%x1f%s%x1f%b%x1e';
    const raw = gitOk(['log', `--grep=Agensh-Integration:`, `-E`, `--format=${logFmt}`, this.integrationBranch], this.root) || '';
    const seen = new Set(store.integrations().map((i) => i.commit_sha).filter(Boolean));
    const recovered = [];
    for (const rec of raw.split('\x1e').map((s) => s.trim()).filter(Boolean)) {
      const [sha, subject, body = ''] = rec.split('\x1f');
      const m = body.match(/Agensh-Integration:\s*(\S+)/);
      if (m && !seen.has(sha)) recovered.push({ sha, integrationId: m[1], subject });
    }
    return recovered;
  }

  head(branch = this.integrationBranch) {
    return git(['rev-parse', branch], this.root);
  }

  // ---- 收尾：集成分支 → main ----
  // run 结束后用户期望成果直接出现在主工作区。只在安全时合并：
  //   ① 用户当前分支 = main（没 checkout 到别处）
  //   ② 工作区干净（不会丢未提交改动）
  //   ③ 领先（fast-forward 可行；分叉则保留 integration 不动，由用户自行处理）
  // 返回 { merged, sha } 或 { merged: false, reason }。
  mergeIntegrationToMain() {
    try {
      const cur = git(['rev-parse', '--abbrev-ref', 'HEAD'], this.root);
      if (cur !== 'main') return { merged: false, reason: `current branch is ${cur}` };
      // 脏区检查必须放过 swarm 自身的运行时产物（.swarm/ 状态库 + worktrees）：
      // 它们永远不会被提交，若算脏则收尾合并几乎必然被拒（用户成果落不了 main）。
      // 用户未提交的其他改动仍然算脏——那是用户自己的 WIP，fast-forward 会冲突。
      const porcelain = gitOk(['status', '--porcelain'], this.root) || '';
      const userDirty = porcelain.split('\n').filter((l) => l.trim() && !/\s\.swarm(\/|$)/.test(l));
      if (userDirty.length) {
        const sample = userDirty[0].slice(0, 60);
        return { merged: false, reason: `working tree not clean (e.g. ${sample})` };
      }
      const mainSha = git(['rev-parse', 'main'], this.root);
      const itSha = git(['rev-parse', this.integrationBranch], this.root);
      if (mainSha === itSha) return { merged: false, reason: 'already up to date' };
      // 确认 integration 包含 main（fast-forward 安全）
      const mergeBase = git(['merge-base', 'main', this.integrationBranch], this.root);
      if (mergeBase !== mainSha) return { merged: false, reason: 'diverged (main has commits not in integration)' };
      git(['merge', '--ff-only', this.integrationBranch], this.root);
      return { merged: true, sha: itSha };
    } catch (e) {
      return { merged: false, reason: String(e.message || e).slice(0, 200) };
    }
  }
}

// 跑门禁测试命令（幂等、限时）。异步：门禁常在集成器里跑数十秒，
// 同步实现会冻住整个 Node 事件循环，把并行 worker 的 API 往返全部卡住。
export function runShell(cmd, cwd, timeoutMs = 120000) {
  return new Promise((resolve) => {
    exec(cmd, {
      cwd, encoding: 'utf8', timeout: timeoutMs, maxBuffer: 32e6,
      windowsHide: true,
    }, (e, stdout, stderr) => {
      // 超时/被杀时 e.code 可能为 null，但 killed=true；统一成非 0 退出码让门禁判失败。
      const code = e ? (typeof e.code === 'number' ? e.code : 1) : 0;
      resolve({ code, output: (stdout || '') + (stderr || '') });
    });
  });
}
