// swarm 监控面板 —— 只读单页，轮询 /status /board /events 自绘。
// 设计约束：
//   - 零依赖、零构建：一个字符串返回给浏览器，不引入任何前端框架/CDN。
//   - 只读：面板绝不 POST，不会干扰正在跑的 swarm（worker 的唯一写通道仍是它们自己的 http 工具）。
//   - 页面内 JS 用字符串拼接（不用模板字符串），避免与外层模板字符串的 ${} 冲突。
export function dashboardHtml() {
  return `<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>swarm dashboard</title>
<style>
  :root {
    --bg: #f3f5f7;
    --surface: rgba(255, 255, 255, 0.94);
    --text: #111827;
    --muted: #6b7280;
    --muted-strong: #4b5563;
    --accent: #111827;
    --success: #1f8f5f;
    --warning: #b7791f;
    --danger: #c24141;
    --line: rgba(15, 23, 42, 0.10);
    --line-strong: rgba(15, 23, 42, 0.18);
    --shadow-md: 0 10px 30px rgba(15, 23, 42, 0.08);
    --radius-xl: 30px;
    --radius-lg: 22px;
    --radius-md: 18px;
    --radius-sm: 14px;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 24px; min-height: 100vh;
    background:
      radial-gradient(1200px 620px at 10% -10%, rgba(99,102,241,0.10), transparent 60%),
      radial-gradient(900px 520px at 100% 0%, rgba(37,99,235,0.08), transparent 55%),
      var(--bg);
    background-attachment: fixed;
    color: var(--text);
    font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif;
    -webkit-font-smoothing: antialiased;
  }
  .wrap { max-width: 1320px; margin: 0 auto; display: grid; gap: 14px; }
  header.top {
    display: flex; align-items: center; justify-content: space-between; gap: 12px;
    padding: 16px 20px; border-radius: var(--radius-lg);
    background: linear-gradient(135deg, rgba(255,255,255,0.98), rgba(248,250,252,0.92));
    border: 1px solid var(--line); box-shadow: var(--shadow-md);
  }
  .brand { display: grid; gap: 2px; }
  .brand h1 { margin: 0; font-size: 18px; letter-spacing: 0.2px; }
  .brand .sub { color: var(--muted); font-size: 12px; }
  .live { display: inline-flex; align-items: center; gap: 8px; font-size: 12px; color: var(--muted-strong); }
  .dot { width: 9px; height: 9px; border-radius: 999px; background: var(--muted); }
  .dot.ok { background: var(--success); box-shadow: 0 0 0 4px rgba(31,143,95,0.15); }
  .dot.bad { background: var(--danger); box-shadow: 0 0 0 4px rgba(194,65,65,0.15); }

  /* ---- 指标卡片：左侧色条 + 悬停抬升 + 层次化排版 ---- */
  .stats { display: grid; gap: 12px; grid-template-columns: repeat(auto-fit, minmax(156px, 1fr)); }
  .stat {
    position: relative; overflow: hidden;
    padding: 15px 16px 15px 19px; border-radius: var(--radius-md);
    background: linear-gradient(160deg, rgba(255,255,255,0.99), rgba(248,250,252,0.94));
    border: 1px solid var(--line); box-shadow: var(--shadow-md);
    display: grid; gap: 6px; align-content: start;
    transition: transform 0.18s ease, box-shadow 0.18s ease, border-color 0.18s ease;
  }
  .stat::before {
    content: ""; position: absolute; left: 0; top: 0; bottom: 0; width: 4px;
    background: var(--tone, var(--muted)); opacity: 0.85;
  }
  .stat:hover {
    transform: translateY(-2px);
    box-shadow: 0 16px 38px rgba(15, 23, 42, 0.13);
    border-color: var(--line-strong);
  }
  .stat .k {
    color: var(--muted); font-size: 11px; font-weight: 600;
    text-transform: uppercase; letter-spacing: 0.8px;
  }
  .stat .v {
    font-size: 27px; font-weight: 700; line-height: 1.1;
    letter-spacing: -0.4px; font-variant-numeric: tabular-nums;
  }
  .stat .v.done { color: var(--success); }
  .stat .v.pending { color: var(--warning); }
  .stat .v.submitted { color: #2563eb; }
  .stat .v.tok { color: #7c3aed; }
  .stat .v.tok-sm { font-size: 17px; font-weight: 600; color: var(--muted-strong); letter-spacing: 0; }
  /* 色条语义：与数值颜色一致，一眼分辨卡片归属 */
  .stat.t-done { --tone: var(--success); }
  .stat.t-pending { --tone: var(--warning); }
  .stat.t-active { --tone: #2563eb; }
  .stat.t-peak { --tone: #6366f1; }
  .stat.t-neutral { --tone: #94a3b8; }
  .stat.t-tok { --tone: #7c3aed; }
  .stat.t-tok-sm { --tone: #a78bfa; }
  /* 主指标（总量/速度）视觉权重更高 */
  .stat.is-hero { grid-column: span 2; }
  @media (max-width: 620px) { .stat.is-hero { grid-column: span 1; } }
  .stat.is-hero .v { font-size: 34px; }

  .grid2 { display: grid; gap: 12px; grid-template-columns: minmax(0, 1.15fr) minmax(0, 1fr); }
  @media (max-width: 900px) { .grid2 { grid-template-columns: 1fr; } }

  .panel {
    border-radius: 20px; padding: 16px; border: 1px solid var(--line);
    background: var(--surface); box-shadow: var(--shadow-md); backdrop-filter: blur(18px);
    display: grid; gap: 12px; align-content: start;
  }
  .panel-head { display: flex; align-items: baseline; justify-content: space-between; gap: 8px; }
  .panel-kicker { color: var(--muted); font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 1.1px; }
  .panel-head h2 { margin: 2px 0 0; font-size: 15px; letter-spacing: -0.1px; }

  .progress-bar { height: 6px; background: rgba(59,130,246,0.12); border-radius: 999px; overflow: hidden; }
  .progress-bar-fill { height: 100%; width: 0%; border-radius: 999px; background: linear-gradient(90deg, #3b82f6, #6366f1); transition: width 0.6s ease-out; }

  .workers { display: grid; gap: 8px; grid-template-columns: repeat(auto-fill, minmax(104px, 1fr)); max-height: 340px; overflow: auto; }
  .wchip {
    position: relative; border: 1px solid var(--line); border-radius: var(--radius-sm); padding: 9px 11px;
    background: linear-gradient(160deg, rgba(255,255,255,0.98), rgba(248,250,252,0.92));
    display: grid; gap: 3px;
    transition: transform 0.16s ease, box-shadow 0.16s ease, border-color 0.16s ease;
  }
  .wchip:hover { transform: translateY(-2px); box-shadow: 0 10px 24px rgba(15,23,42,0.10); }
  .wchip .wn { font-size: 12px; font-weight: 700; }
  .wchip .wt { font-size: 11px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .wchip .wst {
    display: inline-flex; align-items: center; gap: 5px;
    font-size: 11px; font-weight: 600;
  }
  .wchip .wst::before { content: ""; width: 6px; height: 6px; border-radius: 999px; background: currentColor; }
  .wchip.working { border-color: rgba(31,143,95,0.4); box-shadow: 0 0 0 3px rgba(31,143,95,0.07); }
  .wchip.working .wst { color: var(--success); }
  .wchip.idle .wst { color: var(--warning); }
  .wchip.starting .wst { color: #2563eb; }
  .wchip.retired, .wchip.crashed { opacity: 0.55; }
  .wchip.crashed .wst { color: var(--danger); }

  table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
  th, td { text-align: left; padding: 8px; border-bottom: 1px solid var(--line); }
  tbody tr { transition: background 0.14s ease; }
  tbody tr:hover { background: rgba(37,99,235,0.045); }
  th { color: var(--muted); font-weight: 600; font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; }
  td.mono { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 11.5px; }
  .scroll { max-height: 320px; overflow: auto; border-radius: var(--radius-sm); }
  .badge { display: inline-block; padding: 2px 9px; border-radius: 999px; font-size: 11px; font-weight: 600; }
  .badge.pending { background: rgba(183,121,31,0.14); color: var(--warning); }
  .badge.submitted { background: rgba(37,99,235,0.14); color: #2563eb; }
  .badge.done { background: rgba(31,143,95,0.14); color: var(--success); }
  .badge.in_progress { background: rgba(37,99,235,0.14); color: #2563eb; }
  .badge.ok { background: rgba(31,143,95,0.14); color: var(--success); }
  .badge.fail { background: rgba(194,65,65,0.14); color: var(--danger); }
  .ev-type { font-weight: 600; }
  .muted { color: var(--muted); }
  .empty { color: var(--muted); font-size: 12.5px; padding: 8px 2px; }

  /* ---- mind map（任务分解图）：横向树 + 贝塞尔连线 ---- */
  .mm-tools { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .tool-btn {
    min-height: 30px; padding: 0 12px; border-radius: 10px; cursor: pointer;
    border: 1px solid var(--line-strong); background: rgba(255,255,255,0.9); color: var(--text);
    font-size: 12px; transition: transform 0.15s ease;
  }
  .tool-btn:hover { transform: translateY(-1px); }
  .mm-stage {
    position: relative; overflow: auto; max-height: 560px;
    border: 1px solid var(--line); border-radius: var(--radius-sm);
    background: rgba(248,250,252,0.6);
  }
  .mm-canvas { position: relative; }
  .mm-links { position: absolute; left: 0; top: 0; pointer-events: none; }
  .mm-nodes { position: absolute; left: 0; top: 0; }
  .mm-node {
    position: absolute; display: flex; align-items: center; gap: 8px;
    padding: 0 10px; border-radius: var(--radius-sm);
    border: 1px solid var(--line-strong); background: rgba(255,255,255,0.96);
    box-shadow: 0 2px 8px rgba(15,23,42,0.06); overflow: hidden;
  }
  @keyframes mmIn { from { opacity: 0; transform: scale(0.94); } to { opacity: 1; transform: none; } }
  .mm-node.is-new { animation: mmIn 0.35s ease-out both; box-shadow: 0 0 0 3px rgba(37,99,235,0.18); }
  .mm-node.has-kids { cursor: pointer; }
  .mm-node .mm-bar { position: absolute; left: 0; top: 0; bottom: 0; width: 3px; background: var(--muted); }
  .mm-node.pending .mm-bar { background: var(--warning); }
  .mm-node.submitted .mm-bar, .mm-node.in_progress .mm-bar { background: #2563eb; }
  .mm-node.done .mm-bar { background: var(--success); }
  .mm-body { display: grid; gap: 3px; min-width: 0; flex: 1; }
  .mm-title { font-size: 12.5px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .mm-meta { display: flex; align-items: center; gap: 6px; }
  .mm-count { font-size: 11px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .mm-toggle {
    flex: none; width: 20px; height: 20px; border-radius: 999px; cursor: pointer; padding: 0;
    border: 1px solid var(--line-strong); background: rgba(248,250,252,0.95);
    color: var(--muted-strong); font-size: 13px; line-height: 1;
  }
</style>
</head>
<body>
<div class="wrap">
  <header class="top">
    <div class="brand">
      <h1>swarm dashboard</h1>
      <div class="sub" id="sub">connecting…</div>
    </div>
    <div class="live"><span class="dot" id="dot"></span><span id="liveText">—</span></div>
  </header>

  <div class="stats" id="stats"></div>
  <div class="progress-bar"><div class="progress-bar-fill" id="progFill"></div></div>

  <section class="panel">
    <div class="panel-head">
      <div><div class="panel-kicker">token usage</div><h2>本任务 token 用量与速度</h2></div>
      <div class="muted">累计总量 / 瞬时速度 / 平均</div>
    </div>
    <div class="stats" id="tokStats"></div>
  </section>

  <section class="panel">
    <div class="panel-head">
      <div><div class="panel-kicker">mind map</div><h2>任务分解图</h2></div>
      <div class="mm-tools">
        <span class="muted" id="mmMeta">—</span>
        <button class="tool-btn" id="mmExpand" type="button">全部展开</button>
        <button class="tool-btn" id="mmCollapse" type="button">全部折叠</button>
        <button class="tool-btn" id="mmOnlyOpen" type="button">仅未完成</button>
      </div>
    </div>
    <div class="mm-stage" id="mmStage">
      <div class="mm-canvas" id="mmCanvas">
        <svg class="mm-links" id="mmLinks" xmlns="http://www.w3.org/2000/svg"></svg>
        <div class="mm-nodes" id="mmNodes"></div>
      </div>
    </div>
  </section>

  <div class="grid2">
    <section class="panel">
      <div class="panel-head">
        <div><div class="panel-kicker">worker pool</div><h2>Agent 池</h2></div>
        <div class="muted" id="poolMeta">—</div>
      </div>
      <div class="workers" id="workers"></div>
    </section>

    <section class="panel">
      <div class="panel-head">
        <div><div class="panel-kicker">integration</div><h2>最近集成</h2></div>
        <div class="muted" id="intMeta">—</div>
      </div>
      <div class="scroll"><table id="ints"><tbody></tbody></table></div>
    </section>
  </div>

  <div class="grid2">
    <section class="panel">
      <div class="panel-head">
        <div><div class="panel-kicker">board</div><h2>工作项</h2></div>
        <div class="muted" id="boardMeta">—</div>
      </div>
      <div class="scroll"><table id="board"><thead><tr><th>id</th><th>status</th><th>title</th></tr></thead><tbody></tbody></table></div>
    </section>

    <section class="panel">
      <div class="panel-head">
        <div><div class="panel-kicker">events</div><h2>事件流</h2></div>
        <div class="muted" id="evMeta">—</div>
      </div>
      <div class="scroll"><table id="events"><tbody></tbody></table></div>
    </section>
  </div>
</div>

<script>
(function () {
  var lastEventId = 0;
  var eventBuf = [];
  var MAX_EVENTS = 60;

  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  }
  function get(path) {
    return fetch(path, { cache: 'no-store' }).then(function (r) { return r.json(); });
  }
  function fmtTime(ts) {
    if (!ts) return '—';
    var d = new Date(ts);
    return d.toLocaleTimeString('zh-CN', { hour12: false });
  }
  function fmtDur(ms) {
    if (ms == null) return '—';
    var s = Math.floor(ms / 1000);
    var h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
    return (h ? h + 'h ' : '') + m + 'm ' + (s % 60) + 's';
  }
  function fmtNum(n) {
    n = Number(n) || 0;
    if (n >= 1e6) return (n / 1e6).toFixed(2) + 'M';
    if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k';
    return String(n);
  }
  function fmtRate(v) { return (Number(v) || 0).toFixed(1); }

  // token 采样：速度只能靠两次轮询的差分算（服务端只给累计量）。
  // 汇总口径 = 本任务累计 input+output（cacheRead 单独展示，不计入总量，避免重复计数）。
  var tok = { ts: 0, total: 0, speed: 0 };

  function statCards(cards) {
    return cards.map(function (c) {
      var cls = 'stat' + (c.tone ? ' t-' + c.tone : '') + (c.hero ? ' is-hero' : '');
      return '<div class="' + cls + '"><div class="k">' + esc(c.k) + '</div><div class="v ' + (c.cls || '') + '">' + esc(c.v) + '</div></div>';
    }).join('');
  }

  function renderTokens(status) {
    var u = status.tokenUsage || {};
    var inT = u.inputTokens || 0, outT = u.outputTokens || 0;
    var total = inT + outT;
    var turns = u.turns || 0;
    var uptime = status.startedAt ? Date.now() - status.startedAt : 0;

    // 瞬时速度：本次轮询与上次轮询的 token 增量 / 时间增量
    var now = Date.now();
    if (tok.ts && now > tok.ts) {
      tok.speed = (total - tok.total) / ((now - tok.ts) / 1000);
    }
    tok.ts = now; tok.total = total;
    if (tok.speed < 0) tok.speed = 0;

    // 平均：平均每回合 token、全程平均速度（总量 / 运行时长）
    var perTurn = turns ? Math.round(total / turns) : 0;
    var avgSpeed = uptime > 0 ? total / (uptime / 1000) : 0;

    var cards = [
      { k: 'tokens 总量', v: fmtNum(total), cls: 'tok', tone: 'tok', hero: true },
      { k: '速度 tok/s', v: fmtRate(tok.speed), cls: 'tok', tone: 'tok', hero: true },
      { k: '平均 tok/turn', v: fmtNum(perTurn), cls: 'tok', tone: 'tok' },
      { k: '平均 tok/s', v: fmtRate(avgSpeed), cls: 'tok', tone: 'tok' },
      { k: 'in / out', v: fmtNum(inT) + ' / ' + fmtNum(outT), cls: 'tok-sm', tone: 'tok-sm' },
      { k: 'api calls', v: u.apiCalls || 0, cls: 'tok-sm', tone: 'tok-sm' },
      { k: 'cache read', v: fmtNum(u.cacheReadTokens || 0), cls: 'tok-sm', tone: 'tok-sm' },
      { k: 'turns', v: turns, cls: 'tok-sm', tone: 'tok-sm' },
    ];
    document.getElementById('tokStats').innerHTML = statCards(cards);
  }

  function renderStats(st, status) {
    var done = st.done || 0, total = st.total || 0;
    var pct = total ? Math.round(done / total * 100) : 0;
    var cards = [
      { k: 'done', v: done + ' / ' + total, cls: 'done', tone: 'done' },
      { k: 'pending', v: st.pending || 0, cls: 'pending', tone: 'pending' },
      { k: 'submitted', v: st.submitted || 0, cls: 'submitted', tone: 'active' },
      { k: 'active agents', v: (status.active || 0) + ' / ' + (status.maxWorkers || '∞'), cls: '', tone: 'active' },
      { k: 'peak', v: status.peakActive || 0, cls: '', tone: 'peak' },
      { k: 'uptime', v: fmtDur(status.startedAt ? Date.now() - status.startedAt : null), cls: '', tone: 'neutral' },
      { k: 'progress', v: pct + '%', cls: '', tone: 'peak' },
    ];
    document.getElementById('stats').innerHTML = statCards(cards);
    document.getElementById('progFill').style.width = pct + '%';
  }

  function renderWorkers(status) {
    var ws = status.workers || [];
    document.getElementById('poolMeta').textContent = ws.length + ' workers · test: ' + (status.testCmd || 'none');
    if (!ws.length) { document.getElementById('workers').innerHTML = '<div class="empty">暂无 worker</div>'; return; }
    document.getElementById('workers').innerHTML = ws.map(function (w) {
      return '<div class="wchip ' + esc(w.status) + '">'
        + '<div class="wn">' + esc(w.name) + '</div>'
        + '<div class="wst">' + esc(w.status) + '</div>'
        + '<div class="wt">turn ' + esc(w.turn) + (w.idle ? ' · idle ' + esc(w.idle) : '') + '</div>'
        + '</div>';
    }).join('');
  }

  function renderInts(status) {
    var ints = status.integrations || [];
    var ok = ints.filter(function (i) { return i.merged === 1; }).length;
    document.getElementById('intMeta').textContent = ints.length + ' 条 · 成功 ' + ok;
    var tb = document.querySelector('#ints tbody');
    if (!ints.length) { tb.innerHTML = '<tr><td class="empty">暂无集成记录</td></tr>'; return; }
    tb.innerHTML = ints.slice(0, 20).map(function (i) {
      var cls = i.merged === 1 ? 'ok' : 'fail';
      var label = i.merged === 1 ? 'merged' : 'rejected';
      return '<tr><td class="mono">' + esc(i.peer) + '</td><td>' + esc(i.item_id) + '</td>'
        + '<td><span class="badge ' + cls + '">' + label + '</span></td>'
        + '<td class="muted">' + esc(fmtTime(i.ts)) + '</td></tr>';
    }).join('');
  }

  function renderBoard(board) {
    document.getElementById('boardMeta').textContent = 'total ' + board.total + ' · 显示 ' + (board.items || []).length;
    var tb = document.querySelector('#board tbody');
    var items = board.items || [];
    if (!items.length) { tb.innerHTML = '<tr><td class="empty" colspan="3">board 为空</td></tr>'; return; }
    tb.innerHTML = items.map(function (i) {
      return '<tr><td class="mono">' + esc(i.id) + '</td>'
        + '<td><span class="badge ' + esc(i.status) + '">' + esc(i.status) + '</span></td>'
        + '<td>' + esc((i.title || '').slice(0, 60)) + '</td></tr>';
    }).join('');
  }

  function renderEvents() {
    document.getElementById('evMeta').textContent = eventBuf.length + ' 条（最新在上）';
    var tb = document.querySelector('#events tbody');
    if (!eventBuf.length) { tb.innerHTML = '<tr><td class="empty">暂无事件</td></tr>'; return; }
    tb.innerHTML = eventBuf.slice().reverse().map(function (e) {
      var p = e.payload || {};
      var detail = p.name || p.itemId || p.peer || p.id || '';
      return '<tr><td class="muted">' + esc(fmtTime(e.ts)) + '</td>'
        + '<td class="ev-type">' + esc(e.type) + '</td>'
        + '<td class="muted">' + esc(String(detail).slice(0, 48)) + '</td></tr>';
    }).join('');
  }

  // ---------------- mind map（任务分解图） ----------------
  // 数据来自 GET /tree：扁平节点 + parentId 边。前端做横向树布局（叶节点按行堆叠，
  // 父节点取子节点垂直中点），贝塞尔连线，随 /tree 每 2s 刷新"不断细分"。
  var MM_W = 220, MM_H = 48, MM_GX = 64, MM_GY = 12;
  var mm = { nodes: [], roots: [], collapsed: {}, userCollapsed: {}, onlyOpen: false, seen: {}, sig: null };

  function mmShort(s, n) {
    s = String(s == null ? '' : s);
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  }

  function mmRender(force) {
    var canvas = document.getElementById('mmCanvas');
    var nodesEl = document.getElementById('mmNodes');
    var linksEl = document.getElementById('mmLinks');
    var nodes = mm.nodes || [];

    // 签名去抖：树/状态/折叠态没变就不重绘，避免每 2s 重建 DOM（也避免动画反复播放）
    var sig = nodes.map(function (n) { return n.id + ':' + n.status + ':' + n.parentId + (mm.collapsed[n.id] ? 'c' : ''); }).join('|')
      + '|' + (mm.onlyOpen ? 'open' : 'all');
    if (!force && sig === mm.sig) return;
    mm.sig = sig;

    if (!nodes.length) {
      document.getElementById('mmMeta').textContent = '暂无工作项';
      canvas.style.width = '100%';
      canvas.style.height = '120px';
      linksEl.innerHTML = '';
      nodesEl.innerHTML = '<div class="empty" style="padding:14px 16px">board 为空，暂无分解结构</div>';
      return;
    }

    var byId = {}, children = {};
    nodes.forEach(function (n) { byId[n.id] = n; children[n.id] = []; });
    nodes.forEach(function (n) { if (n.parentId && byId[n.parentId]) children[n.parentId].push(n); });

    // 新出现的节点：自动展开其祖先（除非用户手动折叠过），保证"不断细分"立刻可见
    var newSet = {};
    nodes.forEach(function (n) {
      if (mm.seen[n.id]) return;
      newSet[n.id] = 1;
      var cur = n;
      while (cur && cur.parentId && byId[cur.parentId]) {
        if (!mm.userCollapsed[cur.parentId]) delete mm.collapsed[cur.parentId];
        cur = byId[cur.parentId];
      }
    });

    // 可见性（自底向上）：折叠 -> 隐藏整棵子树；仅未完成 -> 隐藏 done 叶节点，
    // 但保留仍有可见子节点的 done 父节点（否则看不到它下面的活）。
    var visCache = {};
    function isVisible(id, stack) {
      if (visCache[id] != null) return visCache[id];
      if (stack[id]) return false; // 环保护
      stack[id] = true;
      var n = byId[id];
      if (!n) { delete stack[id]; return false; }
      if (mm.collapsed[id]) { delete stack[id]; visCache[id] = true; return true; }
      var kids = children[id].filter(function (c) { return isVisible(c.id, stack); });
      var v = mm.onlyOpen ? (n.status !== 'done' || kids.length > 0) : true;
      delete stack[id];
      visCache[id] = v;
      return v;
    }

    // 后序 DFS 布局
    var placed = {}, order = [], row = 0, maxDepth = 0, visiting = {};
    function place(id, depth) {
      if (placed[id]) return placed[id];
      if (visiting[id]) return null; // 环保护
      visiting[id] = 1;
      var n = byId[id];
      if (depth > maxDepth) maxDepth = depth;
      var kids = mm.collapsed[id] ? [] : children[id].filter(function (c) { return isVisible(c.id, {}); });
      var kidPos = [];
      kids.forEach(function (c) { var p = place(c.id, depth + 1); if (p) kidPos.push(p); });
      var y;
      if (!kidPos.length) { y = row * (MM_H + MM_GY); row++; }
      else { y = (kidPos[0].y + kidPos[kidPos.length - 1].y) / 2; }
      var rec = { id: id, x: depth * (MM_W + MM_GX), y: y, node: n, kids: kidPos, totalKids: children[id].length };
      placed[id] = rec;
      order.push(rec);
      delete visiting[id];
      return rec;
    }
    (mm.roots || []).forEach(function (rid) { if (byId[rid] && isVisible(rid, {})) place(rid, 0); });

    if (!order.length) {
      document.getElementById('mmMeta').textContent = nodes.length + ' 节点 · 当前筛选下无可见节点';
      canvas.style.width = '100%';
      canvas.style.height = '120px';
      linksEl.innerHTML = '';
      nodesEl.innerHTML = '<div class="empty" style="padding:14px 16px">当前筛选下没有可见节点</div>';
      return;
    }

    var parts = [], links = [];
    order.forEach(function (p) {
      var st = p.node.status || 'pending';
      mm.seen[p.id] = 1;
      var hasKids = p.totalKids > 0;
      var toggle = hasKids
        ? '<button class="mm-toggle" type="button" aria-label="折叠或展开">' + (mm.collapsed[p.id] ? '+' : '−') + '</button>'
        : '';
      parts.push('<div class="mm-node ' + esc(st) + (hasKids ? ' has-kids' : '') + (newSet[p.id] ? ' is-new' : '') + '"'
        + ' data-id="' + esc(p.id) + '"'
        + ' style="left:' + p.x + 'px;top:' + p.y + 'px;width:' + MM_W + 'px;height:' + MM_H + 'px">'
        + '<span class="mm-bar"></span>'
        + '<div class="mm-body">'
        + '<div class="mm-title" title="' + esc(p.node.title) + '">' + esc(mmShort(p.node.title, 40)) + '</div>'
        + '<div class="mm-meta"><span class="badge ' + esc(st) + '">' + esc(st) + '</span>'
        + (hasKids ? '<span class="mm-count">' + p.totalKids + ' 子项</span>' : '')
        + '</div></div>' + toggle + '</div>');
      p.kids.forEach(function (k) {
        var x1 = p.x + MM_W, y1 = p.y + MM_H / 2, x2 = k.x, y2 = k.y + MM_H / 2;
        var dx = Math.max(24, (x2 - x1) / 2);
        links.push('<path d="M' + x1 + ',' + y1 + ' C' + (x1 + dx) + ',' + y1 + ' '
          + (x2 - dx) + ',' + y2 + ' ' + x2 + ',' + y2
          + '" fill="none" stroke="rgba(15,23,42,0.22)" stroke-width="1.5"/>');
      });
    });

    var w = (maxDepth + 1) * MM_W + maxDepth * MM_GX;
    var h = row * (MM_H + MM_GY) - MM_GY;
    canvas.style.width = w + 'px';
    canvas.style.height = h + 'px';
    linksEl.setAttribute('width', w);
    linksEl.setAttribute('height', h);
    linksEl.setAttribute('viewBox', '0 0 ' + w + ' ' + h);
    linksEl.innerHTML = links.join('');
    nodesEl.innerHTML = parts.join('');

    var doneN = 0;
    nodes.forEach(function (n) { if (n.status === 'done') doneN++; });
    document.getElementById('mmMeta').textContent = nodes.length + ' 节点 · 完成 ' + doneN + ' · 显示 ' + order.length;
  }

  document.getElementById('mmNodes').addEventListener('click', function (ev) {
    var node = ev.target.closest ? ev.target.closest('.mm-node') : null;
    if (!node || !node.classList.contains('has-kids')) return;
    var id = node.getAttribute('data-id');
    if (mm.collapsed[id]) { delete mm.collapsed[id]; delete mm.userCollapsed[id]; }
    else { mm.collapsed[id] = 1; mm.userCollapsed[id] = 1; }
    mmRender(true);
  });
  document.getElementById('mmExpand').addEventListener('click', function () {
    mm.collapsed = {}; mm.userCollapsed = {}; mmRender(true);
  });
  document.getElementById('mmCollapse').addEventListener('click', function () {
    var hasKids = {};
    (mm.nodes || []).forEach(function (n) { if (n.parentId) hasKids[n.parentId] = 1; });
    var c = {}, u = {};
    Object.keys(hasKids).forEach(function (id) { c[id] = 1; u[id] = 1; });
    mm.collapsed = c; mm.userCollapsed = u; mmRender(true);
  });
  document.getElementById('mmOnlyOpen').addEventListener('click', function () {
    mm.onlyOpen = !mm.onlyOpen;
    this.textContent = mm.onlyOpen ? '显示全部' : '仅未完成';
    mmRender(true);
  });

  function setLive(ok, text) {
    document.getElementById('dot').className = 'dot ' + (ok ? 'ok' : 'bad');
    document.getElementById('liveText').textContent = text;
  }

  function tick() {
    Promise.all([
      get('/status').catch(function () { return null; }),
      get('/board?limit=200').catch(function () { return null; }),
      get('/events?after=' + lastEventId).catch(function () { return null; }),
      get('/tree').catch(function () { return null; }),
    ]).then(function (res) {
      var status = res[0], board = res[1], evs = res[2], tree = res[3];
      if (tree) { mm.nodes = tree.nodes || []; mm.roots = tree.roots || []; mmRender(); }
      if (!status) { setLive(false, '连接失败'); return; }
      renderStats(status.stats || {}, status);
      renderTokens(status);
      renderWorkers(status);
      renderInts(status);
      if (board) renderBoard(board);
      if (evs && evs.events) {
        for (var i = 0; i < evs.events.length; i++) {
          var e = evs.events[i];
          eventBuf.push(e);
          if (e.id > lastEventId) lastEventId = e.id;
        }
        if (eventBuf.length > MAX_EVENTS) eventBuf = eventBuf.slice(eventBuf.length - MAX_EVENTS);
        renderEvents();
      }
      var s = status.stats || {};
      document.getElementById('sub').textContent = 'done ' + (s.done || 0) + '/' + (s.total || 0)
        + ' · active ' + (status.active || 0) + ' · peak ' + (status.peakActive || 0);
      setLive(true, 'live · ' + new Date().toLocaleTimeString('zh-CN', { hour12: false }));
    });
  }

  tick();
  setInterval(tick, 2000);
})();
</script>
</body>
</html>`;
}
