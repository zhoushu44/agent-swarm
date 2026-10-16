// HTTP API —— worker agent 的唯一交互通道（localhost only）。
// 协议设计原则（继承自 codex-agensh 并加强）：
//   1. 无状态调用：worker 崩了就崩了，重启后凭 peer 身份重新来过
//   2. 幂等去重：consumeMessages / tryClaim 都是原子操作
//   3. 安全：仅绑定 127.0.0.1，校验 Origin/Host 防跨站
import http from 'node:http';
import { dashboardHtml } from './dashboard.js';

export function createServer(store, { port = 17771, token = null, getStatus = null, dedupJudge = null } = {}) {
  const json = (res, code, obj) => {
    const body = JSON.stringify(obj);
    res.writeHead(code, {
      'Content-Type': 'application/json; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
      'Cache-Control': 'no-store',
    });
    res.end(body);
  };
  const readBody = (req) => new Promise((resolve) => {
    let data = '';
    req.on('data', (c) => { data += c; if (data.length > 5e6) req.destroy(); });
    req.on('end', () => { try { resolve(data ? JSON.parse(data) : {}); } catch { resolve({}); } });
  });

  const server = http.createServer(async (req, res) => {
    // --- 安全检查 ---
    const host = (req.headers.host || '').split(':')[0];
    if (host !== '127.0.0.1' && host !== 'localhost') return json(res, 403, { error: 'host forbidden' });
    const origin = req.headers.origin;
    if (origin) {
      try { if (new URL(origin).host !== req.headers.host) return json(res, 403, { error: 'origin forbidden' }); }
      catch { return json(res, 403, { error: 'bad origin' }); }
    }
    if (token && req.headers['x-swarm-token'] !== token) return json(res, 401, { error: 'unauthorized' });

    const url = new URL(req.url, 'http://127.0.0.1');
    const p = url.pathname;
    const q = Object.fromEntries(url.searchParams);

    try {
      // ---- 读类 ----
      if (req.method === 'GET') {
        if (p === '/health') return json(res, 200, { ok: true });
        // 监控面板（只读）：单页 HTML，前端轮询 /status /board /events 自绘。
        if (p === '/' || p === '/dashboard') {
          const html = dashboardHtml();
          res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Content-Length': Buffer.byteLength(html), 'Cache-Control': 'no-store' });
          return res.end(html);
        }
        // 实时运行态：worker 池 + 统计 + 最近集成。由 runner 注入 getStatus 提供。
        if (p === '/status') {
          if (!getStatus) return json(res, 200, { workers: [], stats: store.stats(), integrations: [] });
          return json(res, 200, getStatus());
        }
        if (p === '/board') {
          // 分页：?status= 过滤 + ?limit= / ?offset= 分页（默认 200，上限 1000）
          const page = store.itemsPaged({
            status: q.status || null,
            limit: q.limit ? Number(q.limit) : 200,
            offset: q.offset ? Number(q.offset) : 0,
          });
          return json(res, 200, page);
        }
        if (p === '/item') {
          const item = store.item(q.id);
          if (!item) return json(res, 404, { error: 'not found' });
          // 捎带该 item 的历史 context（时间正序，最多 20 条）：看板预览 → 按需取详情时一次拿全
          return json(res, 200, { item, history: store.stmts.contextByItem.all(q.id, 20) });
        }
        if (p === '/context/search') {
          return json(res, 200, { entries: store.searchContext(q.q || '', Number(q.limit) || 30) });
        }
        // digest：一次拉齐增量（stats + pending 预览 + 我的认领 + 新 context + 我的消息）。
        // 替代 board+messages+context 三连拉，是省 token 的主路径。since 传上次 digest 的 now。
        if (p === '/digest') {
          return json(res, 200, store.digestFor(q.peer || '', Number(q.since) || 0, Number(q.limit) || 50));
        }
        if (p === '/messages') return json(res, 200, { messages: store.consumeMessages(q.peer || '') });
        if (p === '/events') {
          return json(res, 200, { events: store.eventsAfter(Number(q.after) || 0, 200) });
        }
        // 思维导图数据：扁平节点 + parent 边，前端自建树并做层级布局
        if (p === '/tree') return json(res, 200, store.itemTree());
        if (p === '/stats') return json(res, 200, store.stats());
      }
      // ---- 写类 ----
      else if (req.method === 'POST') {
        const body = await readBody(req);
        if (p === '/claim') {
          const t = store.tryClaim(body.itemId, body.peer, body.leaseSec);
          // 捎带推送：认领成功直接返回完整任务包（item 详情 + 该 item 历史 context），
          // agent 无需再发 /item 和 /context/search。409 仍只报冲突。
          return t ? json(res, 200, store.claimResult(t)) : json(res, 409, { error: 'taken or not pending' });
        }
        if (p === '/renew') {
          return store.renew(body.token) ? json(res, 200, { ok: true }) : json(res, 409, { error: 'lease lost' });
        }
        if (p === '/release') { store.release(body.token); return json(res, 200, { ok: true }); }
        if (p === '/context') {
          const id = store.addContext(body.peer, body.kind, body.text, { itemId: body.itemId, tags: body.tags });
          return json(res, 200, { id });
        }
        if (p === '/message') { store.postMessage(body.from, body.to, body.text, body.itemId); return json(res, 200, { ok: true }); }
        if (p === '/item') {
          const createdBy = body.peer || 'system';
          // LLM 判官去重（仅 worker 创建路径）：seed/plan/测试自带 id 的结构化数据不走这里。
          // 流程（两阶段去重的终审）：
          //   ① findKeyDuplicate：确定性交付物 key 命中（同父项或跨父项文件路径）——
          //      同一文件必然同一交付物，无需打扰 judge，直接折叠（记 scope=parent/global）。
          //   ② 剩余 t:（无文件名）项：召回候选 → dedupJudge.judgeIsDuplicate 终审，
          //      判 duplicate 才合并（宁漏勿错：误合并 = 静默丢活）。
          //   ③ judge 不可用/inconclusive/失败：回退 foldSemantic 词面折叠，
          //      仍不命中才 addItem——任何路径都不丢创建请求。
          // 判官调用带 detail（名字相同但描述不同往往是 update/related，不是 duplicate）。
          if (dedupJudge && !body.id) {
            const parentId = body.parentId || body.parent_id || null;
            const keyDup = store.findKeyDuplicate(parentId, body.title);
            if (keyDup) {
              store.log(createdBy, 'item.deduped', { id: keyDup.item.id, title: body.title, key: keyDup.key, parentId, scope: keyDup.scope });
              return json(res, 200, { item: keyDup.item });
            }
            const candidates = store.semanticCandidates(parentId, body.title);
            if (candidates.length > 0) {
              const verdict = await dedupJudge.judgeIsDuplicate(
                { title: body.title, detail: body.detail || '' },
                candidates.map((c) => ({ id: c.id, title: c.title, detail: c.detail || '' }))
              );
              if (verdict?.duplicate) {
                const dup = store.item(verdict.matchId);
                if (dup && dup.status !== 'done') {
                  store.log(createdBy, 'item.deduped', {
                    id: dup.id, title: body.title, parentId, scope: 'judge',
                    reason: verdict.reason, matchId: verdict.matchId,
                  });
                  return json(res, 200, { item: dup });
                }
              }
              if (!verdict || verdict.inconclusive) {
                // judge 没给结论（网络/限流/解析失败）：保守回退词面折叠
                const folded = store.foldSemantic(parentId, body.title, createdBy);
                if (folded) return json(res, 200, { item: folded });
              }
            }
            const item = store.addItem({
              title: body.title, detail: body.detail || '', kind: body.kind || 'task',
              priority: body.priority || 0, parentId, parent_id: body.parent_id,
              createdBy, semanticFold: false, // 判官流程已处理语义去重，addItem 只做 key 层（①②已在上面查过）
            });
            return json(res, 200, { item });
          }
          const item = store.addItem({
            title: body.title, detail: body.detail || '', kind: body.kind || 'task',
            priority: body.priority || 0, parentId: body.parentId, parent_id: body.parent_id,
            createdBy: body.peer || 'system',
          });
          return json(res, 200, { item });
        }
        if (p === '/submit') {
          // worker 交作业：校验租约 → item 转 submitted 并入队，等集成器跑门禁
          const r = store.submitWork(body.token, body.peer, body.commitSha || null);
          return r ? json(res, 200, { ok: true, ...r }) : json(res, 409, { error: 'lease expired or item not pending' });
        }
      }
      return json(res, 404, { error: 'no such route' });
    } catch (e) {
      return json(res, 500, { error: String(e && e.message ? e.message : e) });
    }
  });

  return new Promise((resolve) => {
    server.listen(port, '127.0.0.1', () => resolve({ server, port }));
  });
}
