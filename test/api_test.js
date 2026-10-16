// board / context / claim HTTP API 单测 —— 直接打 localhost HTTP 端点，验证协议层契约：
//   board:   POST /item（建项/子项）、GET /board（全量 + status 过滤）、GET /item（含捎带历史）、GET /stats
//   context: POST /context（5 类条目 + tags）、GET /context/search（关键词/LIKE 转义/无命中）
//   claim:   POST /claim 捎带任务包（item + history，论文"附带推送"）、409 冲突
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Store } from '../src/store.js';
import { createServer } from '../src/server.js';

const results = [];
function check(name, cond, extra = '') {
  results.push({ name, ok: !!cond });
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${extra ? '  — ' + extra : ''}`);
  return !!cond;
}

async function api(base, method, p, body) {
  const res = await fetch(base + p, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: method === 'GET' ? undefined : JSON.stringify(body || {}),
  });
  const j = await res.json().catch(() => ({}));
  return { status: res.status, ...j };
}

export async function run() {
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), 'swarm-api-'));
  const store = new Store(path.join(repoDir, '.swarm', 'state.db'));
  const PORT = 17895;
  const { server } = await createServer(store, { port: PORT });
  const base = `http://127.0.0.1:${PORT}`;

  try {
    // ================= board =================
    // ---- POST /item ----
    const c1 = await api(base, 'POST', '/item', { peer: 'w1', title: '实现登录', detail: 'OAuth 流程', priority: 5 });
    check('POST /item 创建成功且 pending', c1.status === 200 && c1.item?.status === 'pending', JSON.stringify(c1).slice(0, 120));
    check('POST /item 回读 title/priority', c1.item?.title === '实现登录' && c1.item?.priority === 5);
    check('POST /item created_by 取 peer', c1.item?.created_by === 'w1');

    // ================= context =================
    // 顺序很关键：context 与 claim 的验证必须在给 c1 建子项【之前】完成——
    // 一旦 c1 有了子项它就是"容器"，按 tryClaim 的不变式不可再被认领（返回 409）。
    // ---- POST /context ----
    const x1 = await api(base, 'POST', '/context', {
      peer: 'w1', kind: 'FACT', text: '项目用 JWT 鉴权', itemId: c1.item.id, tags: ['auth', 'backend'],
    });
    check('POST /context 返回 id', x1.status === 200 && typeof x1.id === 'string', JSON.stringify(x1));
    const x2 = await api(base, 'POST', '/context', {
      peer: 'w2', kind: 'FAIL', text: 'OAuth 回调超时 100% 场景失败', tags: ['auth'],
    });
    check('POST /context FAIL 类条目写入', x2.status === 200);
    const x3 = await api(base, 'POST', '/context', { peer: 'w1', kind: 'OBSERVED', text: '依赖版本已锁定' });
    check('POST /context 不带 itemId/tags 也可写', x3.status === 200);
    await api(base, 'POST', '/context', { peer: 'w1', kind: 'CLAIM', text: '采用 RS256 签名' });
    check('context 落库共 4 条', store.searchContext('', 100).length === 4, `n=${store.searchContext('', 100).length}`);

    // ================= claim 捎带（论文"附带推送"契约，需先有 context） =================
    const cl = await api(base, 'POST', '/claim', { itemId: c1.item.id, peer: 'w1' });
    check('POST /claim 成功返回任务包', cl.status === 200 && typeof cl.token === 'string', JSON.stringify(cl).slice(0, 120));
    check('/claim 捎带 item 详情', cl.item?.id === c1.item.id && cl.item?.title === '实现登录');
    check('/claim 捎带租约到期时间', typeof cl.expiresAt === 'number' && cl.expiresAt > Date.now());
    check('/claim 捎带该 item 的历史 context', Array.isArray(cl.history) && cl.history.some((h) => h.text.includes('JWT')), JSON.stringify(cl.history));
    const clConflict = await api(base, 'POST', '/claim', { itemId: c1.item.id, peer: 'w2' });
    check('/claim 二次认领 409', clConflict.status === 409);
    store.release(cl.token); // 释放，不影响后续统计

    const c2 = await api(base, 'POST', '/item', { peer: 'w1', title: '子任务：回调处理', parentId: c1.item.id });
    check('POST /item 带 parentId 建立子项', c2.item?.parent_id === c1.item.id);

    const c3 = await api(base, 'POST', '/item', { peer: 'w2', title: '写文档' });
    check('POST /item 默认 kind=task', c3.item?.kind === 'task');

    // ---- GET /board ----
    const b = await api(base, 'GET', '/board');
    check('GET /board 返回全部 3 项', b.items?.length === 3, `n=${b.items?.length}`);
    check('GET /board 按 priority 降序', b.items?.[0]?.id === c1.item.id);

    const bp = await api(base, 'GET', '/board?status=pending');
    check('GET /board?status=pending 全命中', bp.items?.length === 3 && bp.items.every((i) => i.status === 'pending'));

    // 手动完成一项后再过滤
    store.completeItem(c3.item.id, 'w2', null);
    const bp2 = await api(base, 'GET', '/board?status=pending');
    check('GET /board?status=pending 排除 done', bp2.items?.length === 2);
    const bd = await api(base, 'GET', '/board?status=done');
    check('GET /board?status=done 只返回已完成', bd.items?.length === 1 && bd.items[0].id === c3.item.id);

    // ---- GET /board 分页 ----
    check('GET /board 返回分页元信息', b.total === 3 && b.limit === 200 && b.offset === 0 && b.hasMore === false, JSON.stringify({ total: b.total, limit: b.limit, hasMore: b.hasMore }));
    const pg1 = await api(base, 'GET', '/board?limit=2&offset=0');
    check('分页 limit=2 返回前 2 项', pg1.items?.length === 2 && pg1.total === 3 && pg1.hasMore === true, JSON.stringify({ n: pg1.items?.length, total: pg1.total, hasMore: pg1.hasMore }));
    const pg2 = await api(base, 'GET', '/board?limit=2&offset=2');
    check('分页 offset=2 返回第 3 项且 hasMore=false', pg2.items?.length === 1 && pg2.hasMore === false);
    check('分页两页无重叠且覆盖全集', new Set([...pg1.items, ...pg2.items].map((i) => i.id)).size === 3);
    const pgStatus = await api(base, 'GET', '/board?status=pending&limit=1');
    check('分页 + status 组合：total 为过滤后总数', pgStatus.items?.length === 1 && pgStatus.total === 2, JSON.stringify({ n: pgStatus.items?.length, total: pgStatus.total }));
    const pgCap = await api(base, 'GET', '/board?limit=99999');
    check('分页 limit 上限封顶 1000', pgCap.limit === 1000, `limit=${pgCap.limit}`);
    const pgBad = await api(base, 'GET', '/board?limit=abc&offset=-5');
    check('分页非法参数回退默认', pgBad.limit === 200 && pgBad.offset === 0, JSON.stringify({ limit: pgBad.limit, offset: pgBad.offset }));

    // ---- GET /item ----
    const one = await api(base, 'GET', `/item?id=${c1.item.id}`);
    check('GET /item 命中返回详情', one.status === 200 && one.item?.id === c1.item.id);
    const none = await api(base, 'GET', '/item?id=does-not-exist');
    check('GET /item 不存在返回 404', none.status === 404);

    // ---- GET /stats ----
    const st = await api(base, 'GET', '/stats');
    check('GET /stats 计数正确', st.total === 3 && st.pending === 2 && st.done === 1, JSON.stringify(st));

    // ================= /item 捎带历史 =================
    const oneH = await api(base, 'GET', `/item?id=${c1.item.id}`);
    check('GET /item 捎带历史 context', oneH.status === 200 && Array.isArray(oneH.history) && oneH.history.some((h) => h.text.includes('JWT')), JSON.stringify(oneH.history));

    // ---- GET /context/search ----
    const s1 = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('JWT'));
    check('search 命中关键词', s1.entries?.length === 1 && s1.entries[0].text.includes('JWT'));
    check('search 回读 itemId', s1.entries?.[0]?.item_id === c1.item.id);
    check('search 回读 tags', s1.entries?.[0]?.tags === 'auth,backend');

    const s2 = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('鉴权'));
    check('search 中文命中', s2.entries?.length === 1);

    const s3 = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('zzz不存在zzz'));
    check('search 无命中返回空数组', Array.isArray(s3.entries) && s3.entries.length === 0);

    // limit 生效
    const sLim = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('') + '&limit=2');
    check('search limit 参数生效', sLim.entries?.length === 2, `n=${sLim.entries?.length}`);

    // LIKE 转义：% 与 _ 必须按字面匹配
    const sPct = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('100%'));
    check('search LIKE 转义 %（按字面）', sPct.entries?.length === 1 && sPct.entries[0].text.includes('100%'));

    await api(base, 'POST', '/context', { peer: 'w1', kind: 'FACT', text: '版本 1x0 是特殊的' });
    const sUnd = await api(base, 'GET', '/context/search?q=' + encodeURIComponent('1_0'));
    check('search LIKE 转义 _（不把 _ 当通配）', sUnd.entries?.length === 0, `n=${sUnd.entries?.length}`);
  } finally {
    // 强制关闭 keep-alive 连接，避免 undici 连接句柄在进程退出时触发 libuv 断言
    server.closeAllConnections?.();
    await new Promise((r) => server.close(r));
    store.close();
    fs.rmSync(repoDir, { recursive: true, force: true });
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) throw new Error(`${failed.length} checks failed`);
  return results;
}

if (process.argv[1] && process.argv[1].endsWith('api_test.js')) {
  run().then(() => { console.log('api_test OK'); process.exitCode = 0; })
    .catch((e) => { console.error('api_test FAILED:', e.message); process.exitCode = 1; });
}
