// git 工具 —— worktree 隔离 + 集成分支管理。
// 关键设计（来自 codex-agensh 的教训）：
//   - 每个 worker 一个 worktree（互不干扰）
//   - 集成分支是共享真理：合并时先跑门禁测试，过才 merge
//   - 合并提交带 Agensh-Integration trailer，崩溃后可从 git log 恢复
import { exec, execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// 同步版：只用于初始化/探测等一次性路径（init/head/recover），不进 worker 回合热路径。
export function git(args, cwd, opts = {}) {
  return execFileSync('git', args, {
    cwd, encoding: 'utf8', maxBuffer: 64e6, windowsHide: true,
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

// 祖先校验：sha 是否为 ref 的祖先（merge-base --is-ancestor，exit 0 是 / 1 否 / 128 出错）。
// 集成归因用：worker 提交后分支只应向前走（auto-commit / sync merge 都是加提交），
// 提交时的 sha 必须仍是分支 tip 的祖先；不是 → 分支被 P5 吸收 reset 拉回过（向后移），
// 此时合并该分支是空合并，绝不能记 done。
export async function isAncestor(sha, ref, cwd) {
  if (!sha) return null;
  try { await gitAsync(['merge-base', '--is-ancestor', sha, ref], cwd); return true; }
  catch { return false; }
}

export class Repo {
  constructor(root, { llmApiBase = null, llmApiKey = null, llmModel = null } = {}) {
    this.root = path.resolve(root);
    this.integrationBranch = 'swarm/integration';
    // LLM 仲裁配置（源码冲突时三路合并 + LLM 裁决）：cli.js 从 --api-base/--api-key/--model 传入。
    // 无配置时 _autoResolveWithLLM 直接返回 resolved:false，回退到 merge --abort。
    this.llmApiBase = llmApiBase;
    this.llmApiKey = llmApiKey;
    this.llmModel = llmModel;
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

  // 主分支名（缓存）。真实仓库可能是 main / master / 其他（d5 事故：proj-d5 用
  // master，而 align/收尾合并硬编码 'main' → rev-parse main 抛错静默跳过，
  // 全部产物滞留 swarm/integration，用户工作区一无所有）。
  // 解析优先级：显式设置 > main 存在 > master 存在 > 当前 HEAD 分支名 > 'main'。
  mainBranch() {
    if (this._mainBranch) return this._mainBranch;
    let name = null;
    if (gitOk(['show-ref', '--verify', '--quiet', 'refs/heads/main'], this.root) !== null) name = 'main';
    else if (gitOk(['show-ref', '--verify', '--quiet', 'refs/heads/master'], this.root) !== null) name = 'master';
    else {
      const head = gitOk(['rev-parse', '--abbrev-ref', 'HEAD'], this.root);
      if (head && head !== 'HEAD') name = head;
    }
    this._mainBranch = name || 'main';
    return this._mainBranch;
  }

  // 对齐集成分支与 main（run/init 启动时调用）。
  // 背景：两次 run 之间用户可能直接在 main 上提交（手工改文件 / seed 前的准备提交），
  // 集成分支若停在旧基线会导致：① worker 从旧基线开工，看不到 main 上的新文件；
  // ② 收尾 ff-only 合并因分叉被拒——集成分支上的全部产物滞留，main 上一无所有。
  // 策略：main 严格领先（集成分支是其祖先）→ fast-forward，零冲突风险；
  // 分叉 → 把 main merge 进集成分支保住双方提交，冲突则回滚并如实上报（保持旧行为）；
  // 集成分支领先（上次 run 的产物未回并）→ 不动，本次收尾会正常 ff 回并。
  alignIntegrationToMain() {
    const mb = this.mainBranch();
    const mainSha = gitOk(['rev-parse', mb], this.root);
    const itSha = gitOk(['rev-parse', this.integrationBranch], this.root);
    if (!mainSha || !itSha) return { aligned: false, reason: 'branch missing' };
    if (mainSha === itSha) return { aligned: false, reason: 'up to date' };
    const mergeBase = gitOk(['merge-base', mb, this.integrationBranch], this.root);
    const itDir = this.integrationDir();
    const wtExists = fs.existsSync(path.join(itDir, '.git'));
    if (mergeBase === itSha) {
      // 集成分支若已被 worktree 检出，branch -f 会被 git 拒绝，改在 worktree 里 ff merge。
      if (wtExists) {
        try { git(['merge', '--ff-only', mb], itDir); } 
        catch (e) { return { aligned: false, reason: `ff failed (integration worktree dirty?): ${String(e.message || e).slice(0, 120)}` }; }
      } else {
        git(['branch', '-f', this.integrationBranch, mb], this.root);
      }
      return { aligned: true, mode: 'ff' };
    }
    if (mergeBase === mainSha) return { aligned: false, reason: 'integration ahead (unmerged work from previous run; will merge back at exit)' };
    // 分叉：merge main 进集成分支，保住双方提交；冲突则回滚放弃。
    const dir = wtExists ? itDir : this.ensureIntegrationWorktree();
    try {
      git(['merge', '--no-edit', mb], dir);
      return { aligned: true, mode: 'merge' };
    } catch (e) {
      gitOk(['merge', '--abort'], dir);
      return { aligned: false, reason: `diverged, auto-merge conflicted (kept old baseline): ${String(e.message || e).slice(0, 120)}` };
    }
  }

  // 彻底清除一个 worker 的 worktree + 分支，返回 { ok, alreadyClean }。
  // Windows 教训（d3 实测）：`worktree remove --force` 会因文件锁（Defender/搜索索引器
  // 瞬时占用 .git 下的文件）失败。失败链：目录还在 → prune 清不掉（注册仍有效）→
  // `branch -D` 因"分支被 worktree 检出"失败 → 下次 `worktree add -b` fatal。
  // d4 新教训（三个事实层层叠加）：
  //   ① agent 回合中经 WSL bash 跑 git 会把 worktree/.git 指针改写成 /mnt/c/... 路径；
  //   ② Windows git 验证 .git 文件失败（"not a .git file"）→ remove 直接走不进正路；
  //   ③ .git 指针文件带 Windows 只读属性（git 防误改），node rmSync 对只读文件 EPERM
  //      还静默成功（force:true 吞掉）→ 物理删除也失败 → 残留死循环。
  // 对策：先修指针（attrib -h -r 清属性后重写回 Windows gitdir）让 git remove 走正路；
  // 不行再 attrib 清属性 + rmdir /s /q 物理删除（本机实证：node rmSync 对任意文件
  // 静默失败，rmdir /s /q 稳定成功——这是 Windows 下的主力删除手段）。
  // attrib 走 cmd：node 没有操作 Windows 属性（Hidden/ReadOnly）的原生 API。
  _attrib(args) {
    try { execFileSync('cmd', ['/c', 'attrib', ...args], { windowsHide: true }); return true; }
    catch { return false; }
  }

  _fixWslPointer(dir, name) {
    if (process.platform !== 'win32') return false;
    try {
      const gitFile = path.join(dir, '.git');
      if (!fs.existsSync(gitFile)) return false;
      const content = fs.readFileSync(gitFile, 'utf8');
      if (!content.startsWith('gitdir: /mnt/')) return false;
      // WSL 路径 → Windows 路径：/mnt/c/foo/bar → c:/foo/bar
      const wslPath = content.replace(/^gitdir:\s*/, '').trim();
      const m = wslPath.match(/^\/mnt\/([a-z])\/(.+)$/);
      if (!m) return false;
      const winPath = `${m[1]}:/${m[2]}`;
      // .git 文件带 Hidden 属性（node 对其写入 EPERM），attrib 清掉才能重写
      this._attrib(['-h', '-r', gitFile]);
      fs.writeFileSync(gitFile, `gitdir: ${winPath}\n`);
      return true;
    } catch { return false; }
  }

  async _nukeWorktree(name) {
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    // 快路径：目录和分支都不存在 → 无残留，零子进程往返（stop() 对已回收 worker 的重复调用）
    const br = await gitOkAsync(['branch', '--list', `swarm/w-${name}`], this.root);
    if (!fs.existsSync(dir) && !br) return { ok: true, alreadyClean: true };
    // WSL 指针污染：修复后 git worktree remove 才能走正路
    this._fixWslPointer(dir, name);
    const removed = await gitOkAsync(['worktree', 'remove', '--force', dir], this.root);
    if (fs.existsSync(dir)) {
      if (process.platform === 'win32') {
        // git 删不掉：attrib 清 Hidden/ReadOnly（node 对 Hidden 文件 EPERM），
        // rmdir /s /q 物理删除（本机实证 node rmSync 静默失败、rmdir 稳定成功）
        this._attrib(['-h', '-r', dir]);
        await new Promise((resolve) => {
          execFile('cmd', ['/c', 'rmdir', '/s', '/q', dir], { windowsHide: true }, () => resolve());
        });
        // rmSync 作为第二意见（万一 rmdir 因瞬时锁失败）——force 吞错，靠 existsSync 复查
        if (fs.existsSync(dir)) {
          try { fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch { }
        }
      } else {
        try { fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch { }
      }
      // 目录物理消失后注册才失效，prune 此刻才清得掉
      await gitOkAsync(['worktree', 'prune'], this.root);
    } else if (!removed) {
      // 目录已被外力删除但注册残留 → prune 清掉
      await gitOkAsync(['worktree', 'prune'], this.root);
    }
    await gitOkAsync(['branch', '-D', `swarm/w-${name}`], this.root);
    // P5：备份分支同步回收。分叉线要么已被吸收（reset 重做）、要么已被放弃（retire），
    // 残留备份会在 1000-worker 规模下堆积成上千个死分支。worker 退场 = 它的所有分支退场。
    await gitOkAsync(['branch', '-D', `swarm/backup-${name}`], this.root);
    return { ok: !fs.existsSync(dir) };
  }

  // 为 worker 建 worktree：从集成分支最新位置开新分支。
  // 必须异步：autoscaler 一轮可能连拉 8 个 worker，每个 worktree add 是 ~100-500ms 的子进程，
  // 同步实现会把整个事件循环冻住 8 次，期间所有 worker 的 API 往返全部排队。
  async addWorktree(name, { fresh = true } = {}) {
    // 残留自愈：上次崩溃/清理失败留下的 worktree 或分支，先彻底清除再重建。
    // 任何形态的残留（目录在/不在 × 分支在/不在 × 注册在/不在）都由 _nukeWorktree 收敛到干净态。
    await this._nukeWorktree(name);
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    await gitAsync(['worktree', 'add', '-b', `swarm/w-${name}`, dir, this.integrationBranch], this.root);
    return { name, dir, branch: `swarm/w-${name}` };
  }

  async removeWorktree(name) {
    return this._nukeWorktree(name);
  }

  // ---- 集成分支专用 worktree ----
  // 收尾扫除：清掉不在 .swarm/worktrees 下的散置 worktree 注册（如 agent 回合中
  // 自己 git worktree add 到 %TEMP% 的临时验证目录）。d5 实证：worker pi 在回合里
  // 建了 w1int（Temp 下），run 结束后残留注册——仓库被 external worktree 引用会
  // 阻碍 branch -D、forks/pack 空间泄漏。stop() 后调用，失败不阻断退出。
  pruneForeignWorktrees() {
    try {
      const out = gitOk(['worktree', 'list', '--porcelain'], this.root);
      if (!out) return { pruned: 0 };
      const keep = path.join(this.root, '.swarm', 'worktrees');
      const intDir = this.integrationDir();
      let pruned = 0;
      for (const block of out.split('\n\n')) {
        const m = block.match(/^worktree (.+)$/m);
        if (!m) continue;
        const dir = m[1].trim();
        const p = path.resolve(dir);
        // 保留：主工作区、集成分支 checkout、worker 目录
        if (p === path.resolve(this.root) || p.startsWith(keep) || p === path.resolve(intDir)) continue;
        if (gitOk(['worktree', 'remove', '--force', p], this.root) !== null) pruned++;
        else {
          // 锁死就解除注册：目录多半在 TEMP，物理删除交给系统
          fs.rmSync(p, { recursive: true, force: true });
          if (gitOk(['worktree', 'prune'], this.root) !== null) pruned++;
        }
      }
      return { pruned };
    } catch { return { pruned: 0 }; }
  }

  // 集成分支必须有一个独立 checkout：绝不能在用户主工作区里 `checkout swarm/integration`
  // （会覆盖/丢失用户未提交改动）。集成器所有 merge、门禁测试都在这个 worktree 里做，
  // 主工作区永远停在用户自己的分支上。
  integrationDir() { return path.join(this.root, '.swarm', 'integration'); }
  ensureIntegrationWorktree() {
    const dir = this.integrationDir();
    if (this._itDirReady) return dir; // 已就绪：省掉热路径上的同步 fs.existsSync
    if (fs.existsSync(path.join(dir, '.git'))) { this._itDirReady = true; return dir; }
    if (fs.existsSync(dir)) gitOk(['worktree', 'remove', '--force', dir], this.root); // 清理残留
    // .swarm 目录曾被整删（如用户手动清理/上次 run 中止）时，git worktree 注册表还留着
    // 旧条目，add 会报 "missing but already registered"。prune 清掉失效注册再 add。
    gitOk(['worktree', 'prune'], this.root);
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
      // 冲突文件清单必须在 abort 前抓取：abort 后工作区已复原，U 标记就没了。
      // `ls-files -u` 输出 `mode blob sha<TAB>path`，同一冲突文件占 3 行（3 个 stage），去重取路径。
      const files = await gitOkAsync(['ls-files', '-u'], dir) || '';
      const conflictFiles = [...new Set(files.split('\n')
        .map((l) => l.split('\t').pop()?.trim())
        .filter(Boolean))];
      await gitOkAsync(['merge', '--abort'], dir); // 有冲突：放弃刷新，本回合继续用旧基线（不阻塞 worker）
      // 冲突时也记下纪元：否则每回合都会重试同一个必然冲突的 merge，白烧两次子进程往返。
      // conflictFiles 供 runner 记 skew 计数、挂起持续冲突的 worker（修正版 m4'）。
      return {
        ok: false, conflict: true, epoch: this.integrationEpoch,
        conflictFiles, reason: String(e.message).slice(0, 300),
      };
    }
  }

  // ---- P5 冲突吸收：备份分叉线 + 把 worktree 硬重置回集成分支 ----
  // 背景：syncSkew≥3 意味着 worker 的分叉线已与集成分支实质分叉——继续在自己的
  // 旧基线上做，每次 merge 都在同一批文件上撞。挂起只是拖延，不会让分叉自动变没。
  // 吸收 = 承认现实：分叉线的独有改动备份到 swarm/backup-<name>（不丢工作、可溯源），
  // worktree reset --hard 到集成分支最新 → worker 从干净基线重新吸收全局进度，
  // 带着冲突文件清单（经 CONFLICT 上下文进简报）避开别人刚动过的文件重新干活。
  // 调用方（runner）保证：仅在 redo 预算内调用；超预算走 retire 退场。
  async backupAndResetWorktree(name, { keepBackup = true } = {}) {
    const dir = path.join(this.root, '.swarm', 'worktrees', name);
    if (!fs.existsSync(dir)) return { ok: false, reason: 'no worktree' };
    const branch = `swarm/w-${name}`;
    try {
      // 备份分支指向当前分叉线（w 分支头）。必须先备份再 reset——reset 后分叉头就没了。
      if (keepBackup) await gitOkAsync(['branch', '-f', `swarm/backup-${name}`, branch], this.root);
      // worktree 内 reset --hard：既清工作区脏区、又把检出的 w 分支拉回集成分支。
      // 注意不能用 `branch -f` 改 w 分支——分支被 worktree 检出时 git 拒绝 force update。
      await gitAsync(['reset', '--hard', this.integrationBranch], dir);
      return { ok: true, backup: keepBackup ? `swarm/backup-${name}` : null };
    } catch (e) {
      return { ok: false, reason: String(e && e.message || e).slice(0, 300) };
    }
  }

  // ---- 合并门禁：worker 分支 → 集成分支 ----
  // testCmd 返回 { passed, output }。在集成分支专用 worktree 里 merge --no-ff、跑门禁、过则保留，败则回滚。
  // 关键：全程不碰用户主工作区（不 checkout 到 root），用户随时可继续在自己的分支上工作。
  // 门禁顺序：结构三连检（m3，毫秒级、拦截明显烂提交）→ testCmd（重炮）。
  async integrate(worktree, { testCmd, integrationId, message }) {
    const { branch } = worktree;
    const itDir = this.ensureIntegrationWorktree();
    const ungated = !testCmd;
    const trailer = `Agensh-Integration: ${integrationId}`
      + (ungated ? '\nAgensh-Ungated: true' : ''); // m2：无门禁落地在 git 历史里可追溯
    const msg = (message || `swarm integrate ${branch}`) + `\n\n${trailer}`;
    let beforeSha;
    try {
      // 回滚点：优先用内存镜像（单进程里只有集成器推进集成分支），省掉一次 `rev-parse HEAD`
      // 子进程往返。首次（镜像为空）或镜像与集成分支失联（外部 reset/rebase 过）时，
      // 重新读一次。失联检测：镜像必须仍是当前 HEAD 的祖先（或相等）。
      beforeSha = this.integrationHead;
      if (beforeSha) {
        const cur = await gitOkAsync(['rev-parse', 'HEAD'], itDir);
        // 镜像与 HEAD 不一致且镜像不是 HEAD 的祖先 → 分支被外力拉回，镜像失效
        if (cur && cur !== beforeSha) {
          const anc = await isAncestor(beforeSha, 'HEAD', itDir);
          if (!anc) beforeSha = null;
        }
      }
      if (!beforeSha) beforeSha = await gitAsync(['rev-parse', 'HEAD'], itDir);
      this.integrationHead = beforeSha;
      // 合并前清场：集成 worktree 是 swarm 自有的，但 testCmd（pytest 等）会在里面生成
      // __pycache__/*.pyc 等副产物。一旦这些文件在上次 merge 时被带进树（tracked），
      // 本次 merge 就会报 "Your local changes would be overwritten"（真实事故：5 次
      // integration.failed 全是这个）。reset 清已跟踪改动、clean 清未跟踪副产物。
      await gitAsync(['reset', '--hard', 'HEAD'], itDir);
      await gitAsync(['clean', '-fd'], itDir);
      await gitAsync(['merge', '--no-ff', '-m', msg, branch], itDir);
    } catch (e) {
      // add/add 冲突的高冲突清单文件（package.json 等纯 JSON）走字段级自动合并，
      // 而不是整个回滚——否则每个 worker 各自新建 package.json 会反复撞死集成器。
      const ar = await this._autoResolveJsonConflicts(itDir, msg).catch(() => ({ resolved: false }));
      if (ar.resolved) {
        // JSON 合并成功，进入结构检查
      } else {
        // 非 JSON 的 add/add 冲突（LICENSE、.gitignore、README 等"人人都要建一份"的文件）：
        // 行级去重并集合并（ours 行 + theirs 新增行）。语义上安全——这类文件本来就是
        // 并列条目清单，多一行不会破坏构建。源码冲突绝不走这条路。
        const tr = await this._autoResolveTextConflicts(itDir, msg).catch(() => ({ resolved: false }));
        if (tr.resolved) {
          // 文本清单合并成功，进入结构检查
        } else {
          // 源码文件冲突：三路合并 + LLM 仲裁（需要 apiBase/apiKey 配置）。
          // 限制单次最多 3 个文件，防无限烧 token；LLM 输出必须通过语法校验。
          const lr = await this._autoResolveWithLLM(itDir, msg, {
            apiBase: this.llmApiBase, apiKey: this.llmApiKey, model: this.llmModel, maxFiles: 3,
          }).catch(() => ({ resolved: false }));
          if (!lr.resolved) {
            await gitOkAsync(['merge', '--abort'], itDir);
            return { ok: false, stage: 'merge', reason: String(e.message).slice(0, 400), beforeSha };
          }
        }
      }
    }
    // ---- m3 结构三连检（在重炮 testCmd 之前，毫秒级成本拦截明显烂提交）----
    // ① conflict marker 残留：merge --no-ff 能自动合的文本冲突可能带进 <<<<<<< 标记；
    //    ② 空代码文件：agent 提交空壳文件常是执行中断的信号；
    //    ③ node --check：JS 语法解析。任一失败即 stage='structure'，回滚、不烧 testCmd。
    const struct = await this._structureCheck(itDir);
    let passed = false, output = '';
    if (!struct.ok) {
      await gitOkAsync(['reset', '--hard', beforeSha], itDir);
      return { ok: false, stage: 'structure', reason: struct.reason, beforeSha };
    }
    if (testCmd) {
      // 时序性空跑护栏：src 模块先于 test 文件集成时，门禁必挂（node --test 找不到
      // 文件/目录直接 exit≠0），但每次失败都浪费 worker 一个回合重试。此处预检
      // 集成分支是否已有可跑的测试文件；没有就跳过 testCmd 放行——反正下一轮有
      // 真实测试落地时会重新门禁。
      // 触发条件：testCmd 显式引用了 test/tests/__tests__/spec 目录（说明它依赖文件
      // 系统的测试文件）；`node -e ...` / `exit 1` 这类内联命令不跳过。
      const hasTests = this._hasRunnableTests(itDir, testCmd);
      if (hasTests) {
        try {
          const r = await runShell(testCmd, itDir);
          passed = r.code === 0;
          output = r.output.slice(0, 4000);
        } catch (e) { output = String(e.message).slice(0, 4000); passed = false; }
      } else {
        passed = true;
        output = '(skipped: no test files yet)';
      }
    } else passed = true; // 无测试命令 = 无门禁，直接过（m2 trailer 已标记）
    if (passed) {
      // 集成分支推进成功 → 纪元 +1。worker 下回合的基线刷新据此知道"有新东西要 merge"。
      this.integrationEpoch += 1;
      const sha = await gitAsync(['rev-parse', 'HEAD'], itDir);
      this.integrationHead = sha; // 维护镜像，供下一次合并免读回滚点
      // m5'：改动摘要（diff --stat）。集成器据此广播 PATCH_SUMMARY，其他 worker 无需 diff 就知道动了哪些文件。
      const diffStat = await gitOkAsync(['diff', '--stat', `${beforeSha}..${sha}`], itDir) || '';
      return { ok: true, sha, stage: 'gated', epoch: this.integrationEpoch, diffStat, ungated, beforeSha };
    }
    // 回滚集成：reset 回合并前的 sha（不能用 HEAD~1，no-op 合并时会误删上一个集成）
    await gitOkAsync(['reset', '--hard', beforeSha], itDir);
    return { ok: false, stage: 'test', reason: output, beforeSha };
  }

  // 递归合并两个 JSON 值：对象取 key 并集（冲突键递归），数组去重并集，标量取 incoming。
  _mergeJson(a, b) {
    if (Array.isArray(a) || Array.isArray(b)) {
      if (!Array.isArray(a) || !Array.isArray(b)) return b;
      return [...new Set([...a, ...b])];
    }
    if (a && b && typeof a === 'object' && typeof b === 'object') {
      const out = { ...a };
      for (const k of Object.keys(b)) out[k] = (k in a) ? this._mergeJson(a[k], b[k]) : b[k];
      return out;
    }
    return b === undefined ? a : b;
  }

  // JSON 清单类文件（package.json 等）发生 add/add 或内容冲突时做字段级自动合并。
  // 仅当冲突文件全部落在白名单内才尝试：任一是源码文件就放弃（交回原失败路径）。
  // 成功则 add + commit --no-ff 完成这次 merge，返回 { resolved:true, files }。
  async _autoResolveJsonConflicts(itDir, msg) {
    const AUTO = new Set(['package.json', 'package-lock.json', 'tsconfig.json', 'jsconfig.json']);
    const list = (await gitOkAsync(['diff', '--name-only', '--diff-filter=U'], itDir)) || '';
    const files = list.split('\n').map((s) => s.trim()).filter(Boolean);
    if (files.length === 0 || !files.every((f) => AUTO.has(path.basename(f)))) {
      return { resolved: false, files };
    }
    for (const f of files) {
      const ours = await gitOkAsync(['show', `:2:${f}`], itDir);
      const theirs = await gitOkAsync(['show', `:3:${f}`], itDir);
      let merged;
      try {
        merged = `${JSON.stringify(this._mergeJson(JSON.parse(ours || '{}'), JSON.parse(theirs || '{}')), null, 2)}\n`;
      } catch {
        return { resolved: false, files, reason: `json parse failed: ${f}` };
      }
      fs.writeFileSync(path.join(itDir, f), merged);
      await gitOkAsync(['add', f], itDir);
    }
    await gitAsync(['commit', '--no-ff', '-m', msg], itDir);
    return { resolved: true, files };
  }

  // 文本清单类文件（LICENSE、.gitignore、.npmrc、.editorconfig 等）的 add/add 冲突自动合并：
  // 行级去重并集——ours 的所有行 + theirs 中 ours 没有的行。这类文件本质是并列条目集合，
  // 顺序无关、多一行不破坏语义。仅在所有冲突文件都落在白名单内才尝试。
  async _autoResolveTextConflicts(itDir, msg) {
    const AUTO = new Set(['license', 'license.md', 'license.txt', '.gitignore', '.npmrc', '.editorconfig', '.gitattributes', 'authors', 'contributors', 'changelog.md']);
    const list = (await gitOkAsync(['diff', '--name-only', '--diff-filter=U'], itDir)) || '';
    const files = list.split('\n').map((s) => s.trim()).filter(Boolean);
    if (files.length === 0 || !files.every((f) => AUTO.has(path.basename(f).toLowerCase()))) {
      return { resolved: false, files };
    }
    for (const f of files) {
      const ours = await gitOkAsync(['show', `:2:${f}`], itDir) || '';
      const theirs = await gitOkAsync(['show', `:3:${f}`], itDir) || '';
      const seen = new Set(ours.split('\n'));
      const extra = theirs.split('\n').filter((l) => l.trim() && !seen.has(l));
      const merged = ours.replace(/\n*$/, '\n') + (extra.length ? extra.join('\n') + '\n' : '');
      fs.writeFileSync(path.join(itDir, f), merged);
      await gitOkAsync(['add', f], itDir);
    }
    await gitAsync(['commit', '--no-ff', '-m', msg], itDir);
    return { resolved: true, files };
  }

  // ---------- 源码冲突的 LLM 仲裁 ----------
  // JSON 字段级合并、文本清单行级并集都失败时（说明冲突的是源码文件），
  // 用 git merge-file 做三路合并；若仍有冲突块（<<<<<<< 标记），把冲突块 + 上下文
  // 发给 LLM 裁决。LLM 看到 ours/theirs/base 三个版本，输出合并后的完整文件。
  //
  // 安全措施：
  //   ① 单次最多仲裁 3 个文件（防无限烧 token）
  //   ② LLM 输出的文件必须能通过 node --check（JS 语法校验），否则拒收
  //   ③ 仲裁失败（LLM 超时/输出不合法/语法错误）回退到 merge --abort
  //
  // 返回 { resolved: bool, files: string[], reason?: string }
  async _autoResolveWithLLM(itDir, msg, { apiBase, apiKey, model, maxFiles = 3 } = {}) {
    if (!apiBase || !apiKey) return { resolved: false, reason: 'no apiBase/apiKey for LLM arbitration' };
    const list = (await gitOkAsync(['diff', '--name-only', '--diff-filter=U'], itDir)) || '';
    const files = list.split('\n').map((s) => s.trim()).filter(Boolean);
    if (files.length === 0) return { resolved: false, reason: 'no conflict files' };
    if (files.length > maxFiles) return { resolved: false, reason: `too many conflicts (${files.length} > ${maxFiles})`, files };

    for (const f of files) {
      // 取三路版本：base=共同祖先, ours=当前分支（集成）, theirs=待合并分支
      const base = await gitOkAsync(['show', `:1:${f}`], itDir) || '';
      const ours = await gitOkAsync(['show', `:2:${f}`], itDir) || '';
      const theirs = await gitOkAsync(['show', `:3:${f}`], itDir) || '';

      // 先跑 git merge-file 做三路合并（能自动合的不烧 LLM）
      const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-merge-'));
      const baseFile = path.join(tmpDir, 'base');
      const oursFile = path.join(tmpDir, 'ours');
      const theirsFile = path.join(tmpDir, 'theirs');
      fs.writeFileSync(baseFile, base);
      fs.writeFileSync(oursFile, ours);
      fs.writeFileSync(theirsFile, theirs);

      let merged = null;
      try {
        // git merge-file -p: 输出合并结果到 stdout，冲突块带 <<<<<<< 标记
        const r = await gitOkAsync(['merge-file', '-p', '--marker-size=7', oursFile, baseFile, theirsFile], itDir);
        if (r && !r.includes('<<<<<<<')) {
          // 无冲突块，自动合并成功
          merged = r;
        }
      } catch { /* merge-file 失败，继续走 LLM */ }

      // 有冲突块或 merge-file 失败 → LLM 仲裁
      if (merged === null) {
        const prompt = `You are resolving a git merge conflict in file "${f}".

BASE (common ancestor):
\`\`\`
${base.slice(0, 2000)}
\`\`\`

OURS (integration branch):
\`\`\`
${ours.slice(0, 2000)}
\`\`\`

THEIRS (worker branch):
\`\`\`
${theirs.slice(0, 2000)}
\`\`\`

Output the COMPLETE merged file content (no markdown fence, no explanation). Preserve both changes where possible; if they conflict semantically, prefer OURS structure but incorporate THEIRS additions.`;

        try {
          const res = await fetch(`${apiBase}/chat/completions`, {
            method: 'POST',
            headers: { 'content-type': 'application/json', authorization: `Bearer ${apiKey}` },
            body: JSON.stringify({
              model: model || 'gpt-4',
              messages: [{ role: 'user', content: prompt }],
              max_tokens: 4000,
              temperature: 0,
            }),
            signal: AbortSignal.timeout(30000),
          });
          if (!res.ok) return { resolved: false, reason: `LLM API ${res.status}`, files };
          const j = await res.json();
          const content = j.choices?.[0]?.message?.content?.trim();
          if (!content) return { resolved: false, reason: 'LLM empty response', files };

          // LLM 输出必须能通过语法校验（JS 文件跑 node --check）
          if (f.endsWith('.js') || f.endsWith('.mjs') || f.endsWith('.cjs')) {
            const tmpCheck = path.join(tmpDir, 'check.js');
            fs.writeFileSync(tmpCheck, content);
            try {
              const { execSync } = await import('node:child_process');
              execSync(`node --check "${tmpCheck}"`, { stdio: 'pipe' });
            } catch (e) {
              return { resolved: false, reason: `LLM output failed syntax check: ${e.message.slice(0, 200)}`, files };
            }
          }
          merged = content;
        } catch (e) {
          return { resolved: false, reason: `LLM arbitration failed: ${e.message.slice(0, 200)}`, files };
        } finally {
          // 清理临时目录
          try { fs.rmSync(tmpDir, { recursive: true, force: true }); } catch { }
        }
      }

      // 写回合并结果
      fs.writeFileSync(path.join(itDir, f), merged);
      await gitOkAsync(['add', f], itDir);
    }

    await gitAsync(['commit', '--no-ff', '-m', msg], itDir);
    return { resolved: true, files };
  }

  // ---- 批量集成（P1 吞吐上限）：文件集两两不相交的 K 个 submission 合并为一个
  // 集成候选，跑一次结构检 + 一次 testCmd 一起落地。门禁成本从 O(N×T) 降到
  // O(N/K×T)——testCmd 常是数十秒的重炮，worker 多时它是集成器的吞吐瓶颈。
  // 不相交由 runner 侧分组保证（合并双方都不改同一文件 → merge 不可能文本冲突）。
  // 顺序 merge 每个分支；任一 merge 冲突即整批失败（理论不可能，防御性处理）；
  // 结构检/测试失败 → reset 回 beforeSha，返回失败清单交 runner 逐个回退定位毒项。
  async integrateBatch(entries, { testCmd, integrationId, message }) {
    // entries: [{ branch, peer }]（由 runner 按"文件集不相交"分组后给出）
    if (!entries || entries.length === 0) return { ok: false, stage: 'empty', reason: 'no entries' };
    const itDir = this.ensureIntegrationWorktree();
    const ungated = !testCmd;
    const trailer = `Agensh-Integration: ${integrationId}`
      + (ungated ? '\nAgensh-Ungated: true' : '');
    const msg = `${message || `swarm batch-integrate ${entries.length} branches`}\n\n${trailer}`;
    let beforeSha;
    try {
      // 同 integrate()：失联检测——镜像必须仍是 HEAD 的祖先，否则重新读
      beforeSha = this.integrationHead;
      if (beforeSha) {
        const cur = await gitOkAsync(['rev-parse', 'HEAD'], itDir);
        if (cur && cur !== beforeSha) {
          const anc = await isAncestor(beforeSha, 'HEAD', itDir);
          if (!anc) beforeSha = null;
        }
      }
      if (!beforeSha) beforeSha = await gitAsync(['rev-parse', 'HEAD'], itDir);
      this.integrationHead = beforeSha;
      // 同 integrate()：合并前清场，防 testCmd 副产物（.pyc 等）撞 "local changes would be overwritten"。
      await gitAsync(['reset', '--hard', 'HEAD'], itDir);
      await gitAsync(['clean', '-fd'], itDir);
      // 顺序 merge：前一个落地后一个基于它 merge。不相交文件集保证不会相互冲突。
      for (const e of entries) {
        await gitAsync(['merge', '--no-ff', '-m', `swarm integrate ${e.branch} (batch ${integrationId})\n\n${trailer}`, e.branch], itDir);
      }
    } catch (e) {
      await gitOkAsync(['merge', '--abort'], itDir);
      // 整批回滚到 beforeSha（部分 merge 已落地也要清掉）
      await gitOkAsync(['reset', '--hard', beforeSha], itDir);
      return { ok: false, stage: 'merge', reason: String(e.message).slice(0, 400), beforeSha, failedPeers: entries.map((e) => e.peer) };
    }
    // 一次结构检 + 一次 testCmd 覆盖整批
    const struct = await this._structureCheck(itDir);
    let passed = false, output = '';
    if (!struct.ok) {
      await gitOkAsync(['reset', '--hard', beforeSha], itDir);
      return { ok: false, stage: 'structure', reason: struct.reason, beforeSha, failedPeers: entries.map((e) => e.peer) };
    }
    if (testCmd) {
      // 同 integrate() 的时序性空跑护栏：批次里有 test 文件落地前，整批门禁必挂。
      const hasTests = this._hasRunnableTests(itDir, testCmd);
      if (hasTests) {
        try {
          const r = await runShell(testCmd, itDir);
          passed = r.code === 0;
          output = r.output.slice(0, 4000);
        } catch (e) { output = String(e.message).slice(0, 4000); passed = false; }
      } else {
        passed = true;
        output = '(skipped: no test files yet)';
      }
    } else passed = true;
    if (passed) {
      this.integrationEpoch += 1;
      const sha = await gitAsync(['rev-parse', 'HEAD'], itDir);
      this.integrationHead = sha;
      const diffStat = await gitOkAsync(['diff', '--stat', `${beforeSha}..${sha}`], itDir) || '';
      return { ok: true, sha, stage: 'gated', epoch: this.integrationEpoch, diffStat, ungated, beforeSha, mergedCount: entries.length };
    }
    await gitOkAsync(['reset', '--hard', beforeSha], itDir);
    return { ok: false, stage: 'test', reason: output, beforeSha, failedPeers: entries.map((e) => e.peer) };
  }

  // 时序性空跑护栏用的探测：集成分支当前是否已有可跑的测试文件。
  // 触发口径：testCmd 显式引用了 test/tests/__tests__/spec 目录（才说明它依赖这些
  // 目录下的文件）；探测口径：那些目录里得有 *.test.* / *.spec.* 文件。
  // 自定义/内联 testCmd（node -e ...、exit 1 等）不依赖文件系统，直接返回 true 放行门禁。
  _hasRunnableTests(itDir, testCmd) {
    const dirs = ['test', 'tests', '__tests__', 'spec'];
    const referenced = dirs.filter((d) => new RegExp(`(^|[\\s/"'])${d}([/\\s*"']|$)`, 'i').test(testCmd));
    if (referenced.length === 0) return true; // testCmd 不引用任何测试目录 → 不参与护栏
    for (const d of referenced) {
      const abs = path.join(itDir, d);
      if (!fs.existsSync(abs)) continue;
      try {
        const entries = fs.readdirSync(abs, { recursive: true });
        for (const e of entries) {
          if (/\.(test|spec)\.[cm]?[jt]s$/i.test(String(e))) return true;
        }
      } catch { /* 目录读失败当作无测试，跳过门禁比误挂安全 */ }
    }
    return false;
  }

  // m3 结构三连检：conflict marker 残留 + 空代码文件 + node --check 语法解析。
  // 只扫 merge 真正碰过的文件（diff --name-only beforeSha..HEAD），不是全树——集成分支
  // 可能已有上千文件，全树扫描每合并一次都跑一遍是纯浪费。
  async _structureCheck(itDir) {
    try {
      const head = this.integrationHead; // integrate() 保证此刻 HEAD 未推进、镜像即回滚点
      const changed = await gitAsync(['diff', '--name-only', `${head}..HEAD`], itDir);
      const files = changed.split('\n').map((f) => f.trim()).filter(Boolean);
      // ① conflict marker：直接 grep 文件内容。文件数是 worker 本次改动的规模（个位数到几十），可承受。
      //    必须「开 + 闭成对」才判冲突：`=======` 单独在行首是 markdown/RST setext 标题下划线，
      //    单查它会大面积误报；真实 git 冲突块必有 `<<<<<<< label` 和 `>>>>>>> label` 成对出现。
      for (const f of files) {
        const abs = path.join(itDir, f);
        // ⓪ scratch 文件拦截：agent 调试/探路时生成的临时文件（probe.mjs、scratch/*、tmp_*.mjs、
        //    *.log/*.txt 等）不属于交付物。它们一旦进集成分支就会污染所有 worker 的基线，
        //    还会被 npm test 当测试文件跑（*.test.mjs 误命名）。这类文件名模式几乎不可能
        //    是合法交付物，直接拒整个 merge——worker 下回合在 CONFLICT 简报里看到文件名，
        //    会清理后重新提交。
        if (/(^|\/)(scratch|tmp|temp|probe|debug|test-output)[\/.]/i.test(f)
          || /(?:^|\/)(?:probe|tmp_|temp_|debug_|scratch_)[^/]*\.(?:mjs|js|cjs|ts|py|sh|log|txt)$/i.test(f)
          || /\.(?:log|tmp|bak)$/i.test(f)) {
          return { ok: false, reason: `scratch file in commit: ${f}` };
        }
        if (!fs.existsSync(abs) || fs.statSync(abs).size > 2e6) continue; // 大文件/删除的跳过
        const content = fs.readFileSync(abs, 'utf8');
        if (/^<{7} /m.test(content) && /^>{7} /m.test(content)) {
          return { ok: false, reason: `conflict marker residue in ${f}` };
        }
        // ② 空代码文件：常见代码后缀且零字节 = 执行中断信号。空文件本身未必致命，
        // 但 0 字节的 .js/.ts 几乎必然是 agent 半途而废——放进去会毒害后续 worker 的基线。
        const codeExt = /\.(js|mjs|cjs|ts|tsx|jsx|py|sh|json)$/;
        if (codeExt.test(f) && fs.statSync(abs).size === 0) {
          return { ok: false, reason: `empty code file ${f}` };
        }
      }
      // ③ node --check：只对 JS 系文件做语法解析（py/sh/json 没有等价的单文件快速校验）。
      // 用 git 输出的相对路径（正斜杠，免 Windows 反斜杠转义坑），cwd 已是 itDir。
      const jsFiles = files.filter((f) => /\.(js|mjs|cjs)$/.test(f));
      if (jsFiles.length && jsFiles.length <= 200) {
        for (const f of jsFiles) {
          const r = await runShell(`node --check "${f}"`, itDir, 15000);
          if (r.code !== 0) {
            return { ok: false, reason: `syntax error in ${f}: ${r.output.slice(0, 300)}` };
          }
        }
      }
      return { ok: true };
    } catch (e) {
      // 检查自身出错（比如 diff 失败）不拦门禁——结构检查是防御层，不能变成新的故障点
      return { ok: true, note: String(e.message || e).slice(0, 200) };
    }
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
      const mb = this.mainBranch();
      const cur = git(['rev-parse', '--abbrev-ref', 'HEAD'], this.root);
      if (cur !== mb) return { merged: false, reason: `current branch is ${cur}` };
      // 脏区检查必须放过 swarm 自身的运行时产物（.swarm/ 状态库 + worktrees）：
      // 它们永远不会被提交，若算脏则收尾合并几乎必然被拒（用户成果落不了 main）。
      // 用户未提交的其他改动仍然算脏——那是用户自己的 WIP，fast-forward 会冲突。
      const porcelain = gitOk(['status', '--porcelain'], this.root) || '';
      const userDirty = porcelain.split('\n').filter((l) => l.trim() && !/\s\.swarm(\/|$)/.test(l));
      if (userDirty.length) {
        const sample = userDirty[0].slice(0, 60);
        return { merged: false, reason: `working tree not clean (e.g. ${sample})` };
      }
      const mainSha = git(['rev-parse', mb], this.root);
      const itSha = git(['rev-parse', this.integrationBranch], this.root);
      if (mainSha === itSha) return { merged: false, reason: 'already up to date' };
      // 确认 integration 包含 main（fast-forward 安全）
      const mergeBase = git(['merge-base', mb, this.integrationBranch], this.root);
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
// Windows 坑（d4 实测）：exec() 走 cmd /c "整条命令"，test-cmd 里的内嵌引号
// （如 node --test "test/*.test.js"）会被 cmd 的引号配对规则弄坏——node 收到
// 'c:\' 这种残片直接 MODULE_NOT_FOUND。治本：把命令原样写进临时 .cmd 批文件执行，
// 引号/&&/管道/中文路径全部原样生效，批文件退出码即命令退出码。
export function runShell(cmd, cwd, timeoutMs = 120000) {
  const runRaw = (command, opts) => new Promise((resolve) => {
    exec(command, {
      cwd, encoding: 'utf8', timeout: timeoutMs, maxBuffer: 32e6,
      windowsHide: true, ...opts,
    }, (e, stdout, stderr) => {
      const code = e ? (typeof e.code === 'number' ? e.code : 1) : 0;
      resolve({ code, output: (stdout || '') + (stderr || '') });
    });
  });
  if (process.platform !== 'win32') return runRaw(cmd);
  return (async () => {
    const crypto = await import('node:crypto');
    const file = path.join(cwd, '.swarm', `gate-${crypto.randomBytes(4).toString('hex')}.cmd`);
    fs.mkdirSync(path.join(cwd, '.swarm'), { recursive: true });
    // @echo off：省掉命令回显（门禁输出入库只存 4000 字符，回显会挤掉真实测试输出）。
    // cwd 由 exec 的选项直接给定（Unicode 安全）；批文件里不写 cd——中文路径写进
    // 批文件会被 cmd 按 OEM 代码页解码产生歧义。@exit /b 显式传播命令退出码。
    fs.writeFileSync(file, `@echo off\r\n${cmd}\r\n@exit /b %errorlevel%\r\n`);
    try {
      return await runRaw(`"${file}"`, { timeout: timeoutMs });
    } finally {
      try { fs.rmSync(file, { force: true }); } catch { /* 瞬时文件锁，下次覆盖即可 */ }
    }
  })();
}
