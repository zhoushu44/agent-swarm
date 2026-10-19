// harness 适配层 —— 把任意"能执行一个 prompt 的东西"统一成一个接口。
// 这就是 Agensh "单 agent harness 之上的壳"的落点：
//   claude -p / codex exec / echo（离线测试）→ 全部变成 runTurn(prompt, cwd) => { reply, code }
//
// 每个 turn 起一个新进程（无会话状态，状态全在 store 里）——
// 这是刻意的：状态外置才能让 agent 无状态化、崩溃可恢复、随时换模型。

import { spawn, exec } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

// ---------- 全局在飞 API 闸门（第一性原理） ----------
// 所有 worker 共享同一个 Node 进程，也就共享同一个上游配额。没有闸门时 N 个 worker 会
// 同时把请求打到中转站：1000 agent 即 1000 条并发连接，直接把中转站压死（表现为大面积 503
// "system cpu overloaded"），反而让整体墙钟时间变长。这里做一个进程级信号量：
// 把"在飞的 API 请求数"钳在一个上限内，超出的排队等待，而不是全部同时冲出去。
// 默认上限可用环境变量 SWARM_MAX_INFLIGHT 覆盖（0 或负数 = 不限流，便于 A/B 对照）。
// CLI 的 --max-inflight 通过 setMaxInflight() 覆盖（模块导入后调用，故不能只在顶层读 env）。
const resolveMaxInflight = () => {
  const v = Number(process.env.SWARM_MAX_INFLIGHT);
  return Number.isFinite(v) ? v : 64;
};

export class Semaphore {
  constructor(limit) { this.limit = limit; this.ceiling = limit; this.active = 0; this.queue = []; this._okStreak = 0; this._hiWater = 0; }
  async acquire() {
    if (this.limit <= 0) return; // 不限流
    if (this.active < this.limit) { this.active += 1; this._hiWater = Math.max(this._hiWater, this.active); return; }
    await new Promise((resolve) => this.queue.push(resolve));
    this.active += 1;
    this._hiWater = Math.max(this._hiWater, this.active);
  }
  release() {
    if (this.limit <= 0) return;
    this.active -= 1;
    const next = this.queue.shift();
    if (next) next();
  }
  setLimit(n) {
    this.limit = n;
    // 显式调高上限（如 CLI setMaxInflight）才允许抬高天花板；AIMD 内部回退后的
    // setLimit 不许把天花板顶回去——否则钉顶操作（noteRateLimit）立即被自己撤销。
    if (n > this.ceiling && !this._aimdBacking) this.ceiling = n;
    // 放宽上限时唤醒等待者。注意：queue.shift()() 同步 resolve，但 acquire 内的
    // `await` 之后才会执行 this.active += 1——同步 while 读 this.active 是滞后值，
    // 会把整个队列一次性全唤醒造成超发。正确做法：先算名额，只唤醒那么多个。
    if (n > 0) {
      let canWake = Math.max(0, this.limit - this.active);
      while (canWake-- > 0 && this.queue.length > 0) this.queue.shift()();
    }
  }
  // 自适应并发（AIMD）：撞限流 → 上限乘性回退（0.75x）迅速卸压；连续成功 → 每 8 次
  // 加性回升 1，缓慢逼近天花板。撞限流时把"本轮实际达到的峰值并发"记为新天花板——
  // 服务端在哪个水位开始 503，那里就是它的真实容量；回退后反弹回旧天花板只会反复撞。
  // 而连续成功说明服务端还有余量：天花板每次 +1 缓慢上探，直到再次撞限收敛。
  noteRateLimit() {
    if (this.limit <= 0) return;
    this._okStreak = 0;
    // 503 风暴节流：同一次过载风暴里多个请求几乎同时 503，只让第一个做钉顶+回退，
    // 其余在冷却窗（2s）内直接跳过——否则 N 个并发 503 会连续乘性回退把 limit 打塌到 1。
    const now = Date.now();
    if (this._lastBackoffAt && now - this._lastBackoffAt < 2000) return;
    this._lastBackoffAt = now;
    // 用峰值水位而非当前 active：真实路径先 release() 再 noteRateLimit()，
    // 503 到达时部分并发早已释放，active 会系统性低估撞限水位。
    const hitLevel = Math.max(1, this._hiWater || this.active);
    if (hitLevel < this.ceiling) this.ceiling = hitLevel;
    this._hiWater = 0; // 水位已消费：重置后下一轮重新累积真实峰值，否则旧峰值残留导致后续撞限学不到更低水位
    // 回退基数取 min(limit, ceiling)（即容量水位）：以 64 上限撞到容量 20 时，
    // 退到 15（容量下 25% 卸压）而非 48（仍在容量之上继续压服务端）。
    const backed = Math.max(1, Math.floor(Math.min(this.limit, this.ceiling) * 0.75));
    this._aimdBacking = true;
    this.setLimit(backed);
    this._aimdBacking = false;
  }
  noteOk() {
    if (this.limit <= 0) return;
    this._okStreak += 1;
    if (this._okStreak % 8 !== 0) return;
    // 容量内加性回升：+1（AIMD 的 AI），不越过钉顶水位——撞限后的保守恢复期。
    if (this.limit < this.ceiling) { this.setLimit(this.limit + 1); return; }
    // 顶格持续成功 → 服务端可能已扩容/恢复，天花板动态上探（不永久钉死）。
    // 步长随持续健康度放大：streak≥64 → +2，≥256 → +4。服务器恢复后吞吐
    // 能较快爬回甚至超越撞限前水平；一旦再撞限 streak 立即归零回到最保守 +1。
    const probe = 1 + (this._okStreak >= 64 ? 1 : 0) + (this._okStreak >= 256 ? 2 : 0);
    this.ceiling += probe;
    this.setLimit(this.limit + 1);
  }
}
const apiGate = new Semaphore(resolveMaxInflight());
// 供 CLI / 测试在导入后覆盖上限
export function setMaxInflight(n) {
  const v = Number(n);
  if (Number.isFinite(v)) apiGate.setLimit(v);
  return apiGate.limit;
}
// 供测试/监控读取当前在飞数
export const apiInflight = () => ({ active: apiGate.active, queued: apiGate.queue.length, limit: apiGate.limit });

export async function run(argv, { cwd, timeoutMs = 900000, env = {}, signal, onLine } = {}) {
  // 过闸：CLI harness（pi/claude/codex）每回合 spawn 一个子进程，子进程内的 agent loop
  // 串行发 API 请求 → 1 个活进程 ≤ 1 条在飞请求。占用进程存活期，让 apiInflight 的
  // active 反映真实并发（widget 的"并发 N/限"），max-inflight 对 CLI harness 也真正生效。
  await apiGate.acquire();
  try {
    return await new Promise((resolve) => {
    const child = spawn(argv[0], argv.slice(1), {
      cwd, shell: false,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    // print 模式的 CLI（pi/claude）见到非 tty 的 stdin 会等管道输入到 EOF——
    // 我们从不写 stdin，不主动关掉子进程就会永久挂起。立刻 end 等于"空输入"。
    try { child.stdin.end(); } catch { /* 已销毁则忽略 */ }
    let out = '', err = '', timer = null, timedOut = false, aborted = false;
    // 流式行缓冲：stdout 分片到达，NDJSON 一行一个 JSON 事件。攒到换行就把完整行
    // 喂给 onLine（实时记账用），全量缓存 out 的行为保持不变（结束时仍要整包解析回复）。
    let lineBuf = '';
    const feed = (d) => {
      out += d;
      if (!onLine) return;
      lineBuf += d;
      let i;
      while ((i = lineBuf.indexOf('\n')) >= 0) {
        const line = lineBuf.slice(0, i);
        lineBuf = lineBuf.slice(i + 1);
        try { onLine(line); } catch { /* 记账回调抛错不能杀子进程 */ }
      }
    };
    const kill = () => { try { child.kill(); } catch {} };
    timer = setTimeout(() => { timedOut = true; kill(); }, timeoutMs);
    // 外部中止通道：runner.stop() 时立刻杀掉子进程，避免 drain 卡到超时
    const onAbort = () => { aborted = true; kill(); };
    if (signal) {
      if (signal.aborted) onAbort();
      else signal.addEventListener('abort', onAbort, { once: true });
    }
    child.stdout.on('data', feed);
    child.stderr.on('data', (d) => { err += d; });
    child.on('error', (e) => {
      clearTimeout(timer);
      if (signal) signal.removeEventListener('abort', onAbort);
      resolve({ code: 127, reply: '', error: String(e), aborted });
    });
    child.on('close', (code) => {
      clearTimeout(timer);
      if (signal) signal.removeEventListener('abort', onAbort);
      resolve({ code, reply: out.trim(), stderr: err.trim(), timedOut, aborted });
    });
    });
  } finally {
    apiGate.release();
  }
}

// pi 可执行入口。Windows 下 npm 全局装的 pi 是 pi.cmd shim，
// spawn(shell:false) 解析不了 .cmd 会直接 ENOENT——用 node 跑真实 js 入口绕开。
// 其他平台 pi 在 PATH 里，spawn 单条目即可。
function piProgram() {
  if (process.platform !== 'win32') return ['pi'];
  const cli = path.join(process.env.APPDATA || '', 'npm', 'node_modules',
    '@earendil-works', 'pi-coding-agent', 'dist', 'bundle', 'cli.js');
  if (fs.existsSync(cli)) return [process.execPath, cli];
  return ['pi']; // 非标准路径安装的兜底（交回 PATH 解析，可能 ENOENT 但至少有明确报错）
}

// pi --mode json 的 NDJSON 流式解析器：边喂行边把 usage 增量实时回调（onUsage），
// 结束后 finalize() 返回 { reply, usage }（回合累计）。
// 实测事件语义（pi-json-probe 验证）：
//   message_update.usage  流式期间当前 assistant 消息的"累计"值（首帧从 0 起）
//   message_end.message.usage  该消息最终累计值（与最后一个 update 对齐）
//   turn_end.message          与 message_end 同一条消息——usage 只认 message_end，
//                             再计 turn_end 就是双计（旧版 bug，会让 token 翻倍）。
// 多消息（工具调用多轮）时每条消息各自从 0 累计，apiCalls 按 message_end 计数。
export function createPiJsonStreamParser(onUsage) {
  let reply = '';
  let steps = 0; // assistant 消息数 ≈ 推理步数（pi 无 step 事件，每轮工具调用一条 assistant 消息）
  let lastCum = { input: 0, output: 0, cacheWrite: 0, cacheRead: 0 }; // 当前消息的累计基线
  const usage = { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0 };
  const add = (u, isEnd) => {
    if (!u) return;
    // 累计快照 → 增量：同消息内 usage 单调不减；负差（异常重置）钳 0 不报，避免多计。
    const d = {
      apiCalls: isEnd ? 1 : 0,
      inputTokens: Math.max(0, (Number(u.input) || 0) - lastCum.input),
      outputTokens: Math.max(0, (Number(u.output) || 0) - lastCum.output),
      cacheCreationTokens: Math.max(0, (Number(u.cacheWrite) || 0) - lastCum.cacheWrite),
      cacheReadTokens: Math.max(0, (Number(u.cacheRead) || 0) - lastCum.cacheRead),
    };
    lastCum = { input: Number(u.input) || 0, output: Number(u.output) || 0, cacheWrite: Number(u.cacheWrite) || 0, cacheRead: Number(u.cacheRead) || 0 };
    usage.apiCalls += d.apiCalls;
    usage.inputTokens += d.inputTokens;
    usage.outputTokens += d.outputTokens;
    usage.cacheCreationTokens += d.cacheCreationTokens;
    usage.cacheReadTokens += d.cacheReadTokens;
    if (onUsage && (d.apiCalls || d.inputTokens || d.outputTokens || d.cacheCreationTokens || d.cacheReadTokens)) onUsage(d);
  };
  return {
    line(l) {
      const t = String(l).trim();
      if (!t.startsWith('{')) return;
      let j;
      try { j = JSON.parse(t); } catch { return; }
      if (j.type === 'message_update') { add(j.usage, false); return; }
      if (j.type === 'message_end') {
        const msg = j.message;
        if (!msg || msg.role !== 'assistant') return;
        steps += 1;
        add(msg.usage, true);
        // 下一条消息的 usage 从 0 重新累计：基线清零，否则会漏计/负计
        lastCum = { input: 0, output: 0, cacheWrite: 0, cacheRead: 0 };
        return;
      }
      if (j.type === 'turn_end') {
        const msg = j.message;
        if (!msg || msg.role !== 'assistant') return;
        const texts = (msg.content || []).filter((b) => b.type === 'text').map((b) => b.text);
        if (texts.length) reply = texts.join('\n');
        // usage 不再从 turn_end 取（双计修复，见上）
      }
    },
    finalize() { return { reply, steps, usage: usage.apiCalls ? usage : null }; },
  };
}

// 整包解析（兼容入口）：pi 子进程结束后一次性喂给流式解析器。
export function parsePiJson(stdout) {
  const p = createPiJsonStreamParser(null);
  for (const line of String(stdout).split('\n')) p.line(line);
  return p.finalize();
}

export const harnesses = {
  // ---- 直连 harness：绕过 CLI，直接打 Anthropic 兼容 API（tool-use 循环） ----
  // 为什么需要它：claude -p 每回合要发 ~4400 tokens 的 CLI 系统提示词+工具定义，
  // 中转站按"预扣额度"计费时常常直接 403；裸 API 只发我们的 worker prompt，
  // 且省掉每回合一个 CLI 进程（200-1000 agent 时开销可观）。
  // 工具只给 bash + http 两个，够 worker 跑 git/测试/调协议。
  direct: {
    build(model) { return { model: model || 'gpt-5.5' }; },
    async runTurn({ prompt, cwd, model, timeoutMs, serverPort, peer, maxSteps = 60, apiBase, apiKey, signal, onUsage }) {
      const cfg = loadDirectConfig({ base: apiBase, token: apiKey });
      if (!cfg.token) return { ok: false, reply: '', error: 'direct: missing ANTHROPIC_AUTH_TOKEN' };
      const openai = cfg.apiType === 'openai'; // 新中转站仅 OpenAI 格式（/v1/messages 404）
      // 模型名归一（与 planMission 同规则）：--model 可能带 provider 前缀透传（扩展默认
      // "relay/deepseek-v4-flash"），中转站只认裸名；官方 Anthropic 名不含 "/" 剥前缀为 no-op。
      const bareModel = (model || cfg.model || '').replace(/^[\w.-]+\//, '');
      const b = cfg.base.replace(/\/+$/, '');
      const v1b = /\/v1$/.test(b) ? b : b + '/v1'; // base 可能已带 /v1，避免拼成 /v1/v1
      const url = openai ? v1b + '/chat/completions' : v1b + '/messages';
      const tools = [
        { name: 'bash', description: 'Run a shell command in your worktree (cwd). Returns stdout+stderr. ' +
            'IMPORTANT: the shell is Windows cmd.exe — use `dir`, `type`, `copy`, `echo` (NOT ls/cat/cp/head/grep), chain with `&&`, and there is no `2>/dev/null`. ' +
            'Use this for git, running tests, listing files — but do NOT write files with it (cmd.exe mangles quotes/parens/newlines): use the `write` tool instead. ' +
            'Do NOT use curl/wget for the swarm API — always use the `http` tool instead. Never run commands that wait for interactive input.',
          input_schema: { type: 'object', properties: { cmd: { type: 'string' } }, required: ['cmd'] } },
        // write 工具（第一性原理）：写文件是 worker 最核心的动作，但把文件内容塞进 shell 命令行
        // 本身就是错的接口设计——cmd.exe 会截断多行内容/引号/括号（实测 "only 49 bytes landed"），
        // 让模型反复重写、白烧 4-6 步。这里让内容走工具参数（JSON），完全绕开 shell。
        // 入参沿用 http 工具已验证的"单字符串装 JSON"模式（中转站会丢弃嵌套 object 参数）。
        { name: 'write', description: 'Create or overwrite a file in your worktree. USE THIS INSTEAD OF the shell for writing files — cmd.exe mangles multi-line content, quotes and parentheses.\n' +
            'Pass ONE string field "file" containing JSON: {"path":"src/m3.js","content":"ENTIRE FILE TEXT"}\n' +
            'Example: write({file:"{\\"path\\":\\"src/m3.js\\",\\"content\\":\\"function f(x) {\\\\n  return x;\\\\n}\\\\nmodule.exports = { f };\\\\n\\"}"})\n' +
            'Newlines inside content MUST be \\\\n. The content is written verbatim — no shell escaping.',
          input_schema: { type: 'object', properties: {
            file: { type: 'string', description: 'JSON text like {"path":"src/m3.js","content":"...file text with \\n newlines..."}' },
          }, required: ['file'] } },
        { name: 'http', description: `Call the swarm coordination API at http://127.0.0.1:${serverPort}. THIS IS THE ONLY WAY to talk to the swarm API (never curl).
Pass ONE string field "request" containing the full JSON request. Examples:
  http({request:"{\\"method\\":\\"GET\\",\\"path\\":\\"/board\\"}"})
  http({request:"{\\"method\\":\\"GET\\",\\"path\\":\\"/board\\",\\"status\\":\\"pending\\"}"})
  http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/claim\\",\\"itemId\\":\\"task1\\",\\"peer\\":\\"w1\\"}"})
  http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/submit\\",\\"token\\":\\"<token>\\",\\"peer\\":\\"w1\\",\\"commitSha\\":\\"<sha>\\"}"})
  http({request:"{\\"method\\":\\"POST\\",\\"path\\":\\"/context\\",\\"peer\\":\\"w1\\",\\"kind\\":\\"FACT\\",\\"text\\":\\"...\\",\\"itemId\\":\\"task1\\"}"})
Every POST needs its parameters (itemId, peer, token, commitSha, ...) INSIDE the JSON string.`,
          input_schema: { type: 'object', properties: {
            request: { type: 'string', description: 'JSON text like {"method":"POST","path":"/claim","itemId":"task1","peer":"w1"}' },
          }, required: ['request'] } },
        // finish 是唯一的"结束回合"出口。强制 tool_choice=any 时模型无法靠纯文本结束，
        // 必须显式调 finish —— 这消除了"只说不做"（模型输出一段计划就停）的方差。
        { name: 'finish', description: 'Call this ONLY when your turn is fully done (submitted, or nothing suitable to do). Ends your turn.',
          input_schema: { type: 'object', properties: { summary: { type: 'string', description: 'Short summary of what you did this turn (include the word IDLE if there was no suitable work)' } }, required: ['summary'] } },
      ];
      // prompt caching（Anthropic 计费优化）：给最后一个 tool 加 cache_control，Anthropic 会把
      // "到这个标记为止的前缀"（整个 tools 数组，~2KB）缓存 5 分钟。worker 每回合反复发同样的
      // tools 定义，缓存命中后 input_tokens 按 10% 计费。中转站不支持时该字段被忽略（无副作用）。
      // 注意 OpenAI 格式路径下不传 cache_control（OpenAI 没这个字段）。
      const anthropicTools = tools.map((t, i) => i === tools.length - 1 ? { ...t, cache_control: { type: 'ephemeral' } } : t);
      const messages = [{ role: 'user', content: prompt }];
      const deadline = Date.now() + (timeoutMs || 900000);
      const dbg = process.env.SWARM_DEBUG ? (...a) => console.error(`[direct:${peer}]`, ...a) : () => {};
      let lastText = '';
      let step = 0;
      // 限流退避（第一性原理）：中转站过载时会回 503 "system cpu overloaded"，实测 306/308 个回合
      // 全部撞上它。此时"立刻重试"只会加剧过载（N 个 worker 锁步重打），而"直接失败返回"则白烧
      // 一个 worker turn 预算。正确做法：把限流当"可恢复的等待"——在回合内做有界指数退避 + 抖动
      // （抖动打散 worker 的锁步重试），仍拿不到才把 rateLimited 标记交回 runner 做更长的退避。
      const MAX_RATE_RETRY = 4;
      let rateRetries = 0;
      const isRateLimit = (status, msg) => status === 429 || status === 503 || /overloaded|rate.?limit|too many requests|try again/i.test(String(msg || ''));
      // token 记账：中转站每次响应都带 usage，累计到回合级返回给 runner（省 token 实验的测量基座）。
      // 兼容两套字段名：Anthropic（input_tokens…）与 OpenAI（prompt_tokens…，含缓存命中）。
      const usage = { apiCalls: 0, inputTokens: 0, outputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0 };
      const addUsage = (u) => {
        if (!u) return;
        const d = {
          apiCalls: 1,
          inputTokens: Number(u.input_tokens ?? u.prompt_tokens) || 0,
          outputTokens: Number(u.output_tokens ?? u.completion_tokens) || 0,
          cacheCreationTokens: Number(u.cache_creation_input_tokens) || 0,
          cacheReadTokens: Number(u.cache_read_input_tokens ?? u.prompt_cache_hit_tokens) || 0,
        };
        usage.apiCalls += d.apiCalls;
        usage.inputTokens += d.inputTokens;
        usage.outputTokens += d.outputTokens;
        usage.cacheCreationTokens += d.cacheCreationTokens;
        usage.cacheReadTokens += d.cacheReadTokens;
        // 实时记账：每个 API 响应的 usage 即时回调 runner（流式速度显示的数据源），
        // 回合结束时 runner 会用权威值减去 live 已入账量补差，不会双计。
        try { if (onUsage) onUsage(d); } catch { /* 记账回调抛错不影响回合 */ }
      };
      try {
        for (; step < maxSteps; step++) {
          if (signal?.aborted) return { ok: false, reply: lastText, error: 'aborted', steps: step, usage };
          if (Date.now() > deadline) return { ok: false, reply: lastText, error: 'timeout', steps: step, usage };
          const t0 = Date.now();
          // 过闸：把在飞请求数钳在 MAX_INFLIGHT 内，超出的在这里排队。
          // 只包住 HTTP 往返本身（不含后续 tool 执行），否则闸门会退化成全局串行。
          await apiGate.acquire();
          let res, j;
          try {
            res = await fetch(url, {
              method: 'POST',
              // Anthropic 格式：鉴权头两个都发（中转站鉴权方式换过，双发兼容）。
              // OpenAI 格式：仅 Bearer，多发的头无害但省掉更干净。
              headers: openai
                ? { authorization: `Bearer ${cfg.token}`, 'content-type': 'application/json' }
                : { 'x-api-key': cfg.token, authorization: `Bearer ${cfg.token}`, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
              // OpenAI 工具定义是 {type:'function', function:{name, description, parameters}}，
              // 且 tool_choice 用字符串 "required"（非 Anthropic 的 {type:'any'} 对象）。
              // 模型名剥 provider 前缀（pi 传 relay/xxx，中转站只认裸名）。
              body: JSON.stringify(openai
                ? {
                    model: bareModel, max_tokens: 4096,
                    tools: tools.map((t) => ({ type: 'function', function: { name: t.name, description: t.description, parameters: t.input_schema } })),
                    tool_choice: 'required',
                    messages: openaiMessages(messages),
                  }
                : { model: bareModel, max_tokens: 4096, tools: anthropicTools, tool_choice: { type: 'any' }, messages }),
              signal,
            });
            j = await res.json().catch(() => ({}));
          } finally {
            apiGate.release();
          }
          addUsage(j?.usage);
          dbg(`step ${step}: api ${Date.now() - t0}ms status=${res.status} promptChars=${prompt.length}`);
          if (!res.ok) {
            const msg = j?.error?.message || JSON.stringify(j).slice(0, 300);
            const rl = isRateLimit(res.status, msg);
            // 全局自适应并发：撞限流即乘性回退在飞上限，别让后续请求继续压死服务端。
            if (rl) apiGate.noteRateLimit();
            // 限流：回合内退避重试，且不消耗 step 预算（step-- 抵消 for 的 step++）。
            // 退避 = min(30s, 1.5s * 2^retry) + 0~1.5s 抖动；服务端给了 Retry-After 就听它的。
            // 抖动是关键：N 个 worker 若同时撞限流、又同时重试，会持续把已过载的服务端压死。
            if (rl && rateRetries < MAX_RATE_RETRY) {
              const ra = Number(res.headers.get('retry-after'));
              const base = Number.isFinite(ra) && ra > 0 ? ra * 1000 : 1500 * Math.pow(2, rateRetries);
              const waitMs = Math.min(30000, base) + Math.floor(Math.random() * 1500);
              rateRetries += 1;
              dbg(`rate-limited (${res.status}), retry ${rateRetries}/${MAX_RATE_RETRY} in ${waitMs}ms`);
              await new Promise((r) => setTimeout(r, waitMs));
              if (signal?.aborted) return { ok: false, reply: lastText, error: 'aborted', steps: step, usage };
              step -= 1; // 限流不算一步
              continue;
            }
            // 限流重试耗尽：交回 runner 做更长退避（rateLimited 标记），不当作普通错误烧 turn。
            return { ok: false, reply: lastText, error: `api ${res.status}: ${msg}`, usage, rateLimited: rl };
          }
          apiGate.noteOk(); // 请求成功：加性回升在飞上限（AIMD），缓慢逼近天花板
          // 响应解析：OpenAI 是 choices[0].message{content, tool_calls}，
          // Anthropic 是 content blocks。统一归一化成 {text, toolUses:[{name, input, id}]}，
          // 后续 finish 检测/工具执行走同一套逻辑。
          let lastBlocks = null; // Anthropic 回填时需要原始 blocks
          let toolUses;
          if (openai) {
            const msg = j.choices?.[0]?.message || {};
            if (msg.content) lastText = msg.content;
            toolUses = (msg.tool_calls || []).map((tc) => {
              let input = {};
              try { input = JSON.parse(tc.function?.arguments || '{}'); } catch { /* 模型给了坏 JSON，交回执行层报错 */ }
              return { name: tc.function?.name, id: tc.id, input };
            }).filter((t) => t.name);
            // 回填用的 assistant 消息（保留 tool_calls 原文，OpenAI 协议要求逐字回传）
            if (toolUses.length) lastBlocks = { role: 'assistant', content: msg.content || '', tool_calls: msg.tool_calls };
          } else {
            const blocks = j.content || [];
            for (const b of blocks) if (b.type === 'text' && b.text) lastText = b.text;
            toolUses = blocks.filter((b) => b.type === 'tool_use').map((b) => ({ name: b.name, id: b.id, input: b.input }));
            if (toolUses.length) lastBlocks = blocks;
          }
          dbg(`step ${step}: tools=[${toolUses.map((t) => {
            if (t.name === 'write') return `write:${String(t.input?.file || '').slice(0, 60)}`;
            if (t.name !== 'http') return `${t.name}${t.name === 'bash' ? ':' + String(t.input?.cmd || '').slice(0, 60) : ''}`;
            // 模型用 request 字符串形式时 input.method/path 是空的——必须解析 JSON 才能看清它拉了哪个端点。
            const r = normalizeHttpInput(t.input || {});
            return `http:${r.method} ${r.path}`;
          }).join(', ')}] text=${JSON.stringify(String(lastText).slice(0, 100))}`);
          if (toolUses.length === 0) return { ok: true, reply: lastText || '(no reply)', error: null, steps: step + 1, usage };
          // finish 调用 → 正常结束回合
          const fin = toolUses.find((t) => t.name === 'finish');
          if (fin) return { ok: true, reply: fin.input?.summary || lastText || '(finished)', error: null, steps: step + 1, usage };
          let submitted = false;
          // 同一 assistant 消息里的多个 tool_use 是模型的"并行意图"——串行 await 会让
          // 每个工具的延迟相加（一个 turn 里 2 次 http 就多等一个往返）。这里并行跑，
          // 结果按原顺序回填（tool_result 顺序必须与 tool_use 一致）。
          // OpenAI 的 tool 消息用 tool_call_id 字段，Anthropic 用 tool_use_id——统一都发。
          const results = await Promise.all(toolUses.map(async (tu) => {
            let out;
            if (tu.name === 'bash') out = await runBash(tu.input?.cmd || '', cwd);
            else if (tu.name === 'write') out = runWrite(tu.input || {}, cwd);
            else if (tu.name === 'http') {
              const req = normalizeHttpInput(tu.input || {});
              const r = await runHttp(serverPort, req, cfg.token);
              out = r.text;
              dbg(`  http ${req.method} ${req.path} raw=${JSON.stringify(tu.input).slice(0, 300)} -> ${String(out).slice(0, 200)}`);
              // 提交成功 = 回合目标已达成（submit 是协议第 5 步，也是最后一步）。
              // 立刻结束回合，避免 agent 在"已提交"后继续空转把步数烧光。
              if (r.status >= 200 && r.status < 300 && /^\/submit\b/.test(req.path)) submitted = true;
            } else out = `unknown tool: ${tu.name}`;
            return { type: 'tool_result', tool_use_id: tu.id, tool_call_id: tu.id, content: String(out).slice(0, 8000) };
          }));
          if (submitted) return { ok: true, reply: lastText || '(submitted)', error: null, steps: step + 1, usage };
          // 回填：Anthropic 是 assistant blocks + user tool_result[]；
          // OpenAI 是 assistant {content, tool_calls} + 每个 tool_call 一条 {role:'tool', tool_call_id, content}。
          if (openai) {
            messages.push(lastBlocks);
            for (const r of results) {
              messages.push({ role: 'tool', tool_call_id: r.tool_call_id, content: r.content });
            }
          } else {
            messages.push({ role: 'assistant', content: lastBlocks });
            messages.push({ role: 'user', content: results });
          }
        }
        return { ok: true, reply: lastText || '(max steps reached)', error: null, steps: step, usage };
      } catch (e) {
        return { ok: false, reply: lastText, error: String(e && e.message || e), steps: step, usage };
      }
    },
  },
  // ---- 真实 harness：pi CLI（print 模式，推荐底座） ----
  // 对应官方集成四模式中的 print：pi -p "prompt" 跑完输出最终回复即退出。
  // 关键 flag 组合（与 swarm"状态外置、每回合新进程"的设计对齐）：
  //   --no-session        回合不落会话文件（swarm 状态全在 store，pi 会话只是垃圾）
  //   --model             选模型（支持 provider/id 语法，如 anthropic/claude-…）
  //   --tools read,bash,edit,write[,powershell]
  //                       整体替换默认选择（注意：普通名字列表是白名单，须列全；
  //                       千万别再叠 -xt —— 那是"排除"，先白后除会把工具全清零）
  //   --no-extensions     扩展/MCP/skill 全关，减少启动开销与不确定性
  //   --offline           关掉模型目录刷新等后台网络活动（模型经 --model 显式给）
  //   --no-context-files  不自动读 AGENTS.md/CLAUDE.md（worker prompt 已是完整协议）
  // 工具选择：read/edit/write 供写码，bash 走 Git Bash 跑 git/测试，
  // Windows 原生进程再给 powershell（pwsh 优先，回退 Windows PowerShell），
  // worker 调 swarm API 用 bash 里的 curl 或 powershell 的 Invoke-RestMethod。
  // 认证：模型 provider 配在 ~/.pi/agent/models.json（relay = Anthropic 兼容中转站，
  // apiKey 用 $SWARM_PI_API_KEY 插值），这里每回合注入该变量——密钥不落盘。
  // token 来源与 direct harness 相同：--api-key/环境变量 > ~/.claude/settings.json。
  pi: {
    build(model) { return { model: model || '' }; },
    async runTurn({ prompt, cwd, model, timeoutMs, signal, env = {}, apiBase, apiKey, onUsage }) {
      const cfg = loadDirectConfig({ base: apiBase, token: apiKey });
      const tools = process.platform === 'win32'
        ? 'read,bash,edit,write,powershell' : 'read,bash,edit,write';
      // --mode json：NDJSON 事件流，message_update/message_end 带 usage（input/output/cacheRead），
      // runner 的 token 记账才有数据。纯文本模式 stdout 只剩最终回复，usage 全丢。
      const argv = [...piProgram(), '-p', '--no-session', '--no-extensions', '--no-context-files',
        '--offline', '--tools', tools, '--mode', 'json'];
      if (model) argv.push('--model', model);
      argv.push('--', prompt);
      // 流式实时记账：逐行喂解析器，usage 增量即时回调 runner（DeepSeek 客户端式 tok/s 的数据源），
      // 回合结束后 finalize 拿权威累计——两路都出自同一解析器，天然不双计。
      const parser = createPiJsonStreamParser(onUsage || null);
      const r = await run(argv, { cwd, timeoutMs, signal, env: { ...env, SWARM_PI_API_KEY: cfg.token || '' }, onLine: (l) => parser.line(l) });
      const parsed = parser.finalize();
      return {
        ok: r.code === 0 && !r.timedOut && !r.aborted && !!parsed.reply,
        reply: parsed.reply || r.reply || r.stderr || '(empty)',
        error: r.error || (r.timedOut ? 'timeout' : null),
        steps: parsed.steps,
        usage: parsed.usage,
      };
    },
  },
  // ---- 真实 harness：claude CLI（当前机器已配中转站） ----
  claude: {
    build(model) { return { model: model || 'gpt-5.5' }; },
    async runTurn({ prompt, cwd, model, timeoutMs, signal }) {
      const r = await run(['claude', '-p', prompt, '--model', model, '--dangerously-skip-permissions'],
        { cwd, timeoutMs, signal, env: { CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1' } });
      return { ok: r.code === 0 && !r.timedOut && !r.aborted, reply: r.reply || r.stderr || '(empty)', error: r.error || (r.timedOut ? 'timeout' : null) };
    },
  },
  // ---- 真实 harness：codex CLI（备用，需登录） ----
  codex: {
    build(model) { return { model: model || 'gpt-5' }; },
    async runTurn({ prompt, cwd, model, timeoutMs, signal }) {
      const r = await run(['codex', 'exec', '--ephemeral', '--skip-git-repo-check', '-s', 'workspace-write', '--model', model, prompt],
        { cwd, timeoutMs, signal });
      return { ok: r.code === 0 && !r.timedOut && !r.aborted, reply: r.reply || r.stderr || '(empty)', error: r.error || (r.timedOut ? 'timeout' : null) };
    },
  },
  // ---- 离线测试 harness：不调 API，按脚本执行协议动作 ----
  echo: {
    build(script) { return { script }; },
    async runTurn({ prompt, cwd, script, serverPort, peer, turn }) {
      // 脚本语法：每行一个动作（支持 {{var}} 变量插值）
      //   sleep <ms>                    等待
      //   claim <itemId>                调 POST /claim，成功则把 token 存入 {{token}}
      //   api <METHOD> <path> <json>    调本集群 API（可含 {{token}}）
      //   file <path> <content>         写文件（模拟写码）
      //   commit <msg>                  git add -A && commit（模拟交作业）
      //   say <text>                    最终回复
      const fs = await import('node:fs');
      const { execFileSync } = await import('node:child_process');
      const lines = (script || '').split('\n').map((l) => l.trim()).filter(Boolean);
      const base = `http://127.0.0.1:${serverPort}`;
      const vars = { peer: peer || '', turn: turn == null ? '' : String(turn) };
      const interp = (s) => s.replace(/\{\{(\w+)\}\}/g, (_, k) => (vars[k] === undefined ? '' : String(vars[k])));
      let reply = '(echo done)';
      const call = async (method, p, body) => {
        const res = await fetch(base + p, {
          method,
          headers: { 'Content-Type': 'application/json' },
          body: method === 'GET' ? undefined : JSON.stringify(body || {}),
        });
        const j = await res.json().catch(() => ({}));
        return { status: res.status, ...j };
      };
      for (const raw of lines) {
        const line = interp(raw);
        if (line.startsWith('sleep ')) { await new Promise((r) => setTimeout(r, Number(line.slice(6)))); continue; }
        if (line.startsWith('claim ')) {
          const itemId = line.slice(6).trim();
          const r = await call('POST', '/claim', { itemId, peer: vars.peer });
          if (r.token) vars.token = r.token;
          reply = JSON.stringify(r).slice(0, 500);
          continue;
        }
        if (line.startsWith('api ')) {
          const [_, method, p, ...rest] = line.split(' ');
          const body = rest.join(' ') ? JSON.parse(rest.join(' ')) : {};
          const r = await call(method, p.startsWith('/') ? p : '/' + p, body);
          reply = JSON.stringify(r).slice(0, 500);
          continue;
        }
        if (line.startsWith('file ')) {
          const m = line.match(/^file (\S+) (.+)$/);
          if (m) {
            const dir = m[1].split('/').slice(0, -1).join('/');
            if (dir) fs.mkdirSync(cwd + '/' + dir, { recursive: true });
            fs.writeFileSync(cwd + '/' + m[1], m[2] + '\n');
          }
          continue;
        }
        if (line.startsWith('commit')) {
          const msg = line.slice(6).trim() || `wip: ${vars.peer}`;
          try {
            execFileSync('git', ['add', '-A'], { cwd, encoding: 'utf8', windowsHide: true });
            execFileSync('git', ['-c', 'user.name=swarm', '-c', 'user.email=swarm@local', 'commit', '-m', msg],
              { cwd, encoding: 'utf8', windowsHide: true });
          } catch { /* 无改动或已提交，忽略 */ }
          continue;
        }
        if (line.startsWith('say ')) { reply = line.slice(4); continue; }
      }
      return { ok: true, reply, error: null };
    },
  },
};

export function getHarness(name) {
  const h = harnesses[name];
  if (!h) throw new Error(`unknown harness: ${name}`);
  return h;
}

// 读中转站配置。优先级：显式入参 > 环境变量 > ~/.swarm/api.json（swarm 专用，
// 不动 ~/.claude/settings.json）> ~/.claude/settings.json 的 env 段。
// 凭据只从上述来源读取，绝不硬编码——测试脚本也复用本函数（缺口⑥ 明文 API key）。
export function loadDirectConfig({ base, token } = {}) {
  token = token || process.env.SWARM_API_KEY || process.env.ANTHROPIC_AUTH_TOKEN || process.env.ANTHROPIC_API_KEY || '';
  base = base || process.env.SWARM_API_BASE || process.env.ANTHROPIC_BASE_URL || '';
  let model = process.env.SWARM_MODEL || '';
  let apiType = process.env.SWARM_API_TYPE || ''; // 'openai' = chat/completions；空 = Anthropic /messages
  if (!token || !base || !model || !apiType) {
    try {
      const p = path.join(process.env.USERPROFILE || process.env.HOME || '', '.swarm', 'api.json');
      const s = JSON.parse(fs.readFileSync(p, 'utf8'));
      token = token || s.apiKey || '';
      base = base || s.baseUrl || '';
      model = model || s.model || '';
      apiType = apiType || s.apiType || '';
    } catch { /* 无配置文件 */ }
  }
  if (!token || !base || !model) {
    try {
      const p = path.join(process.env.USERPROFILE || process.env.HOME || '', '.claude', 'settings.json');
      const s = JSON.parse(fs.readFileSync(p, 'utf8'));
      token = token || s?.env?.ANTHROPIC_AUTH_TOKEN || '';
      base = base || s?.env?.ANTHROPIC_BASE_URL || '';
      model = model || s?.model || '';
    } catch { /* 无配置文件 */ }
  }
  return { token, base: base || 'https://api.anthropic.com', model: model || 'gpt-5.5', apiType };
}

// write 工具实现：内容来自工具参数（JSON），完全绕开 shell。
// 入参兼容两种形态：{file:"<JSON>"}（首选）或已解析的 {path, content}。
function runWrite(input, cwd) {
  let path_ = '', content = '';
  if (typeof input.file === 'string' && input.file.trim()) {
    try {
      const j = JSON.parse(input.file);
      path_ = String(j.path || '');
      content = j.content == null ? '' : String(j.content);
    } catch (e) {
      return `write failed: "file" must be JSON like {"path":"src/m3.js","content":"..."} — parse error: ${e.message}`;
    }
  } else {
    path_ = String(input.path || '');
    content = input.content == null ? '' : String(input.content);
  }
  if (!path_) return 'write failed: missing "path" in the JSON.';
  const abs = path.isAbsolute(path_) ? path_ : path.join(cwd, path_);
  // 防越界：只允许写在 worktree 内（worker 不该碰别处）。
  const rel = path.relative(cwd, abs);
  if (rel.startsWith('..') || path.isAbsolute(rel)) return `write failed: path escapes your worktree: ${path_}`;
  try {
    fs.mkdirSync(path.dirname(abs), { recursive: true });
    fs.writeFileSync(abs, content);
    return `wrote ${path_} (${Buffer.byteLength(content)} bytes, ${content.split('\n').length} lines)`;
  } catch (e) {
    return `write failed: ${e.message}`;
  }
}

// 在 worktree 里跑 shell 命令（模拟 agent 的终端能力）。
// 必须异步：所有 worker 共享同一个 Node 事件循环，execSync 会把整个进程冻住，
// 导致其它 worker 的 API 响应/工具调用全部排队——这是"并行 turn 比单跑慢 4 倍"的根因。
function runBash(cmd, cwd) {
  if (!cmd) return Promise.resolve('(empty command)');
  const dbg = process.env.SWARM_DEBUG ? (...a) => console.error('[direct:bash]', ...a) : () => {};
  const t0 = Date.now();
  return new Promise((resolve) => {
    exec(cmd, { cwd, encoding: 'utf8', timeout: 60000, maxBuffer: 16e6, windowsHide: true }, (e, stdout, stderr) => {
      if (!e) {
        dbg(`${Date.now() - t0}ms ok cmd=${cmd.slice(0, 120)}`);
        resolve(stdout || '(no output)');
        return;
      }
      const out = (stdout || '') + (stderr || '');
      // killed=true 通常是 timeout:60000 触发——交互式命令（node REPL / git 编辑器 / 等待 stdin）会卡满。
      dbg(`${Date.now() - t0}ms EXIT ${e.code} killed=${e.killed || false} cmd=${cmd.slice(0, 120)}`);
      resolve(`exit ${e.code ?? '?'}${e.killed ? ' (TIMEOUT/KILLED)' : ''}:\n${out || e.message}`);
    });
  });
}

// OpenAI chat/completions 的消息序列转换：内部 messages 形态（Anthropic 风格：
// {role:'user', content:string} / {role:'assistant', content:blocks} / user 带 tool_result[]）
// → OpenAI 风格。http 工具的 tool_result 在 OpenAI 里变成 {role:'tool', tool_call_id, content}。
// 初始 messages 只有 [{role:'user', content:prompt}]，回填走 runTurn 内的分支，这里做兜底转换。
function openaiMessages(messages) {
  const out = [];
  for (const m of messages) {
    if (m.role === 'user' && Array.isArray(m.content)) {
      // tool_result 数组（Anthropic 形态）→ 拆成独立 tool 消息
      for (const b of m.content) {
        if (b.type === 'tool_result') out.push({ role: 'tool', tool_call_id: b.tool_call_id || b.tool_use_id, content: String(b.content ?? '') });
      }
      continue;
    }
    out.push(m);
  }
  return out;
}

// 规范化 http 工具入参。中转站是 OpenAI→Anthropic 桥，会丢弃嵌套 object 参数，
// 所以工具 schema 用单个字符串字段 "request"（JSON 文本）。同时兼容：
//   - 旧/扁平写法：{method, path, itemId, peer, ...} 顶层键
//   - 嵌套写法：{method, path, body:{...}}
function normalizeHttpInput(input) {
  let method = String(input.method || 'GET').toUpperCase();
  let p = input.path ? String(input.path) : '';
  const body = {};
  const put = (k, v) => { if (v !== undefined && v !== null && v !== '') body[k] = v; };

  // 1) request 字符串（首选）
  if (typeof input.request === 'string' && input.request.trim()) {
    try {
      const j = JSON.parse(input.request);
      method = String(j.method || method).toUpperCase();
      if (j.path) p = String(j.path);
      for (const [k, v] of Object.entries(j)) if (!['method', 'path'].includes(k)) put(k, v);
    } catch { /* 非法 JSON，忽略，退回其它来源 */ }
  }
  // 2) 嵌套 body
  if (input.body && typeof input.body === 'object') for (const [k, v] of Object.entries(input.body)) put(k, v);
  // 3) 扁平顶层键
  for (const [k, v] of Object.entries(input)) {
    if (['method', 'path', 'request', 'body'].includes(k)) continue;
    put(k, v);
  }
  return { method, path: p, body };
}

// ---------- mission → board 种子（一次性规划调用） ----------
// 缺口⑨：此前跑 swarm 必须先人工手写 seed.json。这里用一次 LLM 调用把 mission
// 直接翻译成首批工作项，让"给一句话就能开跑"。
// 刻意只做"顶层任务拆分"（3~8 项），不细拆——细拆是 worker 每回合的活（见 worker prompt
// 第 2 步的 split 逻辑）。规划器若把任务拆太细，反而会固化错误的分解，压死 worker 的自主性。
export async function planMission({ mission, conventions = '', count = 6, apiBase, apiKey, model } = {}) {
  const cfg = loadDirectConfig({ base: apiBase, token: apiKey });
  if (!cfg.token) throw new Error('planMission: missing API token (set ANTHROPIC_AUTH_TOKEN or ~/.swarm/api.json)');
  const openai = cfg.apiType === 'openai';
  const b = cfg.base.replace(/\/+$/, '');
  const v1b = /\/v1$/.test(b) ? b : b + '/v1';
  const url = openai ? v1b + '/chat/completions' : v1b + '/messages';
  // 模型名归一：pi 的 models.json 用 "provider/model" 形式（如 relay/deepseek-v4-flash），
  // 但中转站只认裸模型名。--model 常被直接透传（扩展默认值即带前缀），这里统一剥掉。
  const bareModel = (model || cfg.model || '').replace(/^[\w.-]+\//, '');
  const n = Math.max(1, Math.min(Number(count) || 6, 20));
  const prompt = `You are the planning step of a decentralized agent swarm. Break the MISSION below into ${n} (max ${n}) top-level work items that independent worker agents can each claim and finish.

MISSION:
${mission}
${conventions ? `\nPROJECT CONVENTIONS:\n${conventions}\n` : ''}
Rules:
- Each item = ONE concrete deliverable, sized for a single agent turn (or a small group of sub-items it can split itself).
- Give each item a title that names ONLY its concrete deliverable (prefer a real file path with extension, e.g. "src/auth.js"). No verbs like "Create"/"创建", no parenthetical notes.
- Order items so foundational work (scaffolding, config, shared interfaces) comes before dependent work.
- AGGREGATOR/BARREL files LAST: any file whose only job is to re-export or aggregate other modules (index.js barrels, README.md, package entry points) MUST be the FINAL item — never first. If it is created before its dependencies exist, the swarm will waste turns re-editing it as each dependency lands (observed: barrel written early exports only 12 of 15 modules → 6 re-export test failures).
- Do NOT over-decompose: 3~${n} items is plenty. Workers will split further as needed.
- Keep items balanced in size: if the mission lists many concrete things (files/functions), split them evenly across items rather than creating one giant item.
- Respond with ONLY a JSON object, no prose, no markdown fence:
{"items":[{"title":"...","detail":"what done looks like + how to verify","kind":"task","priority":5}]}`;

  let lastErr = null;
  for (let attempt = 0; attempt < 4; attempt++) {
    await apiGate.acquire();
    let res, j;
    try {
      res = await fetch(url, {
        method: 'POST',
        headers: openai
          ? { authorization: `Bearer ${cfg.token}`, 'content-type': 'application/json' }
          : { 'x-api-key': cfg.token, authorization: `Bearer ${cfg.token}`, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
        body: JSON.stringify({ model: bareModel, max_tokens: 4096, messages: [{ role: 'user', content: prompt }] }),
      });
      j = await res.json().catch(() => ({}));
    } finally { apiGate.release(); }
    if (!res.ok) {
      const msg = j?.error?.message || j?.error || JSON.stringify(j).slice(0, 300);
      const rl = res.status === 429 || res.status === 503 || /overloaded|rate.?limit|too many/i.test(String(msg));
      lastErr = new Error(`planner api ${res.status}: ${msg}`);
      if (!rl) throw lastErr;
      await new Promise((r) => setTimeout(r, Math.min(30000, 1500 * Math.pow(2, attempt)) + Math.floor(Math.random() * 1500)));
      continue;
    }
    // 文本提取：OpenAI choices[0].message.content；Anthropic content blocks。
    const text = openai
      ? String(j.choices?.[0]?.message?.content || '')
      : (j.content || []).filter((x) => x.type === 'text').map((x) => x.text).join('\n');
    const items = parsePlannerItems(text);
    if (items.length === 0) { lastErr = new Error(`planner returned no parseable items; raw: ${text.slice(0, 300)}`); continue; }
    return { items, usage: j.usage || null, raw: text };
  }
  // 兜底：LLM 4 次都返回不可解析内容时，从 mission 文本里硬提取文件路径拆解。
  // 保证即使 planner 完全失灵也能启动（拆得粗糙但能用，worker 会自行细拆）。
  console.warn('[swarm] planner parse failed, falling back to regex extraction');
  const fallback = mission.match(/(?:src|test|lib|app|docs?|config)\/[\w./-]+\.\w+/g) || [];
  const unique = [...new Set(fallback)];
  if (unique.length > 0) {
    const items = unique.slice(0, n).map((title) => ({ title, detail: `Implement ${title} as described in the mission.`, kind: 'task', priority: 5 }));
    return { items, usage: null, raw: '(regex fallback)' };
  }
  throw lastErr || new Error('planMission failed');
}

// ---------- LLM 判官：重复子任务终审（两阶段去重的第二阶段） ----------
// 背景：词面/token 去重（store.dedupKey 的三级折叠）只能兜住"确定性交付物相同"和
// "token 子集"两类；"实现登录" vs "实现用户登录功能" vs "支持账号密码登录" 这类
// 同义变体词面挡不住，用户实测 100 个 item 里大量"名字不同但任务是差不多的"。
// 工业标准解法（CrewAI consolidation / PD3 / 语义缓存论文）：廉价词法召回候选 →
// LLM judge 终审，只有 judge 判 duplicate 才合并，宁漏勿错（误合并 = 静默丢活）。
// 本工厂返回一个 judge 对象：无凭据时返回 null（调用方走词面回退），有凭据时
// judgeIsDuplicate 做一次极小的 LLM 调用（四选一：duplicate/related/update/distinct）。
// 任何失败（网络/限流/解析）都返回 { duplicate: false, inconclusive: true }，
// 绝不因 judge 故障丢掉创建请求——调用方收到 inconclusive 后自行决定回退策略。
export function createDedupJudge({ apiBase, apiKey, model } = {}) {
  let cfg = null;
  try {
    cfg = loadDirectConfig({ base: apiBase, token: apiKey });
  } catch {
    return null; // 配置读取失败（如 api.json 格式损坏）——判官不可用
  }
  if (!cfg.token || !cfg.base) return null; // 无凭据：判官不可用，调用方走词面回退
  const openai = cfg.apiType === 'openai';
  const b = cfg.base.replace(/\/+$/, '');
  const v1b = /\/v1$/.test(b) ? b : b + '/v1';
  const url = openai ? v1b + '/chat/completions' : v1b + '/messages';
  // 与 planMission 相同的模型名归一：--model relay/xxx 透传时剥掉 provider 前缀。
  const bareModel = (model || cfg.model || '').replace(/^[\w.-]+\//, '');
  if (!bareModel) return null;

  async function judgeIsDuplicate(newItem, candidates = []) {
    if (!candidates || candidates.length === 0) return { duplicate: false, matchId: null, reason: 'no candidates', inconclusive: false };
    const fmt = (x) => `#${x.id}  title: ${x.title}${x.detail ? `\n        detail: ${x.detail}` : ''}`;
    const prompt = `You are the dedup judge of a decentralized agent swarm. A worker wants to create a new work item. Existing candidate items (same parent, still open) are listed below. Decide whether the NEW item is a DUPLICATE of any candidate.

NEW ITEM:
  title: ${newItem.title}${newItem.detail ? `\n  detail: ${newItem.detail || ''}` : ''}

CANDIDATES:
${candidates.map(fmt).join('\n')}

Decision rules (choose exactly one):
- "duplicate": some candidate describes THE SAME deliverable — same target files/artifacts, equivalent acceptance criteria. Different wording is fine ("实现登录" vs "实现用户登录功能"). Return the candidate #id.
- "related": candidates touch the same feature area but are distinct pieces of work (e.g. "实现登录" vs "实现注册"). NOT a duplicate.
- "update": the new item is a strict improvement/extension of an existing one (e.g. adds OAuth on top of existing login). NOT a duplicate — keep both.
- "distinct": no meaningful overlap.
When unsure, prefer NOT duplicate: a missed merge costs one extra item; a wrong merge silently drops work.
Respond with ONLY a JSON object, no prose, no markdown fence:
{"decision":"duplicate","matchId":"<candidate id>","reason":"<one short sentence>"}`;

    let lastErr = null;
    for (let attempt = 0; attempt < 2; attempt++) { // 判官只需轻量重试：它不是关键路径，失败可回退
      await apiGate.acquire();
      let res, j;
      try {
        res = await fetch(url, {
          method: 'POST',
          headers: openai
            ? { authorization: `Bearer ${cfg.token}`, 'content-type': 'application/json' }
            : { 'x-api-key': cfg.token, authorization: `Bearer ${cfg.token}`, 'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
          body: JSON.stringify({ model: bareModel, max_tokens: 300, messages: [{ role: 'user', content: prompt }] }),
        });
        j = await res.json().catch(() => ({}));
      } catch (e) {
        lastErr = e; // 网络异常：finally 释放闸门后进入下一次轻试（或返回 inconclusive）
        continue;
      } finally {
        apiGate.release(); // 单点释放：成功/异常路径都恰好一次，不泄漏也不超放
      }
      if (!res.ok) {
        const msg = j?.error?.message || j?.error || JSON.stringify(j).slice(0, 300);
        const rl = res.status === 429 || res.status === 503 || /overloaded|rate.?limit|too many/i.test(String(msg));
        lastErr = new Error(`judge api ${res.status}: ${msg}`);
        if (!rl) break;
        await new Promise((r) => setTimeout(r, Math.min(10000, 1000 * Math.pow(2, attempt)) + Math.floor(Math.random() * 800)));
        continue;
      }
      const text = openai
        ? String(j.choices?.[0]?.message?.content || '')
        : (j.content || []).filter((x) => x.type === 'text').map((x) => x.text).join('\n');
      const verdict = parseJudgeVerdict(text);
      if (verdict) {
        if (verdict.decision === 'duplicate' && verdict.matchId) {
          const hit = candidates.find((c) => String(c.id) === String(verdict.matchId));
          if (hit) return { duplicate: true, matchId: hit.id, reason: verdict.reason || '', inconclusive: false };
          return { duplicate: false, matchId: null, reason: `judge matched unknown id ${verdict.matchId}`, inconclusive: true };
        }
        return { duplicate: false, matchId: null, reason: verdict.reason || verdict.decision, inconclusive: false };
      }
      lastErr = new Error(`judge returned unparseable verdict; raw: ${text.slice(0, 200)}`);
    }
    return { duplicate: false, matchId: null, reason: `judge unavailable: ${String(lastErr && lastErr.message || lastErr)}`, inconclusive: true };
  }

  return { judgeIsDuplicate };
}

// 判官输出解析：与 parsePlannerItems 同一套容错（裸 JSON → 围栏 → 首尾大括号），
// 但校验 decision 必须是四个合法值之一，防模型自由发挥。
function parseJudgeVerdict(text) {
  const s = String(text || '').trim();
  const tryParse = (t) => {
    try {
      const j = JSON.parse(t);
      const d = String(j?.decision || '').toLowerCase();
      if (!['duplicate', 'related', 'update', 'distinct'].includes(d)) return null;
      return { decision: d, matchId: j?.matchId || null, reason: j?.reason || '' };
    } catch { return null; }
  };
  let v = tryParse(s);
  if (!v) {
    const fence = s.match(/```(?:json)?\s*([\s\S]*?)```/);
    if (fence) v = tryParse(fence[1].trim());
  }
  if (!v) {
    const a = s.indexOf('{'), z = s.lastIndexOf('}');
    if (a !== -1 && z > a) v = tryParse(s.slice(a, z + 1));
  }
  return v;
}

// 规划器输出解析：模型常把 JSON 包在 ``` 围栏里或前后带解释文字，这里都容错。
function parsePlannerItems(text) {
  const s = String(text || '').trim();
  const tryParse = (t) => { try { const j = JSON.parse(t); return Array.isArray(j?.items) ? j.items : (Array.isArray(j) ? j : null); } catch { return null; } };
  let items = tryParse(s);
  if (!items) {
    const fence = s.match(/```(?:json)?\s*([\s\S]*?)```/);
    if (fence) items = tryParse(fence[1].trim());
  }
  if (!items) {
    const a = s.indexOf('{'), z = s.lastIndexOf('}');
    if (a !== -1 && z > a) items = tryParse(s.slice(a, z + 1));
  }
  if (!Array.isArray(items)) return [];
  return items
    .map((it) => ({
      title: String(it?.title || '').trim(),
      detail: String(it?.detail || '').trim(),
      kind: String(it?.kind || 'task'),
      priority: Number(it?.priority) || 0,
    }))
    .filter((it) => it.title);
}

// 代理 worker 调用 swarm 协调 API（把协议从 shell curl 里解放出来）。
async function runHttp(serverPort, { method, path: p, body }, token) {
  method = String(method || 'GET').toUpperCase();
  p = p && p.startsWith('/') ? p : '/' + (p || '');
  if (method === 'GET' && body && Object.keys(body).length > 0) {
    const qs = new URLSearchParams(body).toString();
    p += (p.includes('?') ? '&' : '?') + qs;
  }
  const headers = { 'Content-Type': 'application/json' };
  if (token) headers['X-Swarm-Token'] = token;
  try {
    const res = await fetch(`http://127.0.0.1:${serverPort}${p}`, {
      method, headers, body: method === 'GET' ? undefined : JSON.stringify(body || {}),
    });
    const text = await res.text();
    return { status: res.status, text: `${res.status} ${text.slice(0, 8000)}` };
  } catch (e) {
    return { status: 0, text: `http error: ${String(e && e.message || e)}` };
  }
}
