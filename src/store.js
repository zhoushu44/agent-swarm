// swarm store —— 所有状态的核心。借鉴 Agensh / codex-agensh 的 append-only 思路：
// board / context / events 全部只追加；派生表（work_items 的 status、claims 的租约）由
// 各入口在同一个事务里与事件一起写，因此派生表本身就是权威状态，events 是并行的审计线。
// 崩溃恢复因此分两步：① 审计事件流与派生表是否一致（_replay）；② 清理崩溃残留
// （expireOrphanClaims 释放死掉的 worker 留下的活跃租约；recoverIntegrations 补回
// "已 merge 但没落库"的集成记录）。
//
// 规模考虑（200-1000 agent）：单文件 SQLite 用 node:sqlite（Node 24 内置、同步、零依赖）承载，
// 但全部访问走本模块的原子事务，将来要分片时只改这一层。
import { DatabaseSync } from 'node:sqlite';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const WORK_ITEM_FIELDS = 'id, parent_id, title, detail, kind, priority, status, created_by, created_at, done_at, not_before';
const CONTEXT_FIELDS = 'id, ts, peer, kind, item_id, text, tags';

// 拆解幂等键（第一性原理：拆解是 swarm 扩大并行度的唯一机制，重复拆解是最大的收敛杀手）。
// 精确 title 去重挡不住 LLM 的标题变体——EXP4 实测同一父项下出现 91/64 个重复子项：
//   "创建 src/m1.js" / "创建 src/m1.js (f_1(x)=x+1)" / "创建 src/m1.js（导出 f_1(x)=x+1）" ...
// 全是同一交付物，却因修饰语不同被判为不同项 → 父项永远等不齐 → 任务卡死。
// 归一化策略：优先抽取标题里的"确定性交付物名"（带扩展名的文件路径 token），
// 同一交付物的各种写法折叠成同一个 key；没有文件名时回退到"小写 + 去空白/标点"。
const ARTIFACT_RE = /[A-Za-z0-9_@.\/\\-]+\.[A-Za-z][A-Za-z0-9]{0,5}\b/g;
export function dedupKey(title) {
  const s = String(title || '');
  const hits = (s.match(ARTIFACT_RE) || [])
    .map((f) => f.replace(/\\/g, '/').replace(/^\.\//, '').toLowerCase());
  const uniq = [...new Set(hits)].sort();
  if (uniq.length > 0) return 'f:' + uniq.join('|');
  const norm = s.toLowerCase().replace(/[^a-z0-9\u4e00-\u9fff]+/g, '');
  return 't:' + (norm || s.toLowerCase());
}

// 无文件名项（t: 键）的语义折叠用 token 集：ASCII 按词切、CJK 逐字切。
// 判定用"子集"而不是"相似度阈值"：只有一项的 token 全被另一项包含时才折叠。
// 于是 "实现登录" 能折进 "实现用户登录功能"，而 "实现登录" 与 "实现注册" 不会互相吞并。
export function titleTokens(title) {
  const out = new Set();
  for (const m of String(title || '').toLowerCase().matchAll(/[a-z0-9]+|[\u4e00-\u9fff]/g)) out.add(m[0]);
  return out;
}
function isSubset(a, b) {
  for (const x of a) if (!b.has(x)) return false;
  return true;
}

export class Store {
  constructor(dbPath) {
    fs.mkdirSync(path.dirname(dbPath), { recursive: true });
    this.db = new DatabaseSync(dbPath);
    // node:sqlite 没有 pragma() 便捷方法，用 prepare().get() 执行 pragma
    this.db.prepare('PRAGMA journal_mode = WAL').get();
    this.db.prepare('PRAGMA synchronous = NORMAL').get();
    this.db.prepare('PRAGMA busy_timeout = 10000').get();
    this._migrate();
    // 预编译常用语句
    this.stmts = {
      insertEvent: this.db.prepare(
        'INSERT INTO events (ts, peer, type, payload) VALUES (?, ?, ?, ?)'
      ),
      getEventsAfter: this.db.prepare(
        'SELECT id, ts, peer, type, payload FROM events WHERE id > ? ORDER BY id LIMIT ?'
      ),
      insertItem: this.db.prepare(
        `INSERT INTO work_items (${WORK_ITEM_FIELDS}, dedup_key) VALUES (@id, @parent_id, @title, @detail, @kind, @priority, @status, @created_by, @created_at, @done_at, @not_before, @dedup_key)`
      ),
      getItem: this.db.prepare(`SELECT ${WORK_ITEM_FIELDS} FROM work_items WHERE id = ?`),
      // 某父项下的子项数：拆解后的 item 变成"容器"，不可再被认领（否则 N 个 worker 会各自重拆一遍）
      childCount: this.db.prepare('SELECT COUNT(*) AS n FROM work_items WHERE parent_id = ?'),
      // 拆解幂等用：同一父项下按归一化 key 查已存在的子项（精确 title 挡不住 LLM 标题变体）
      // 用 `IS` 而非 `=`：根级项 parent_id 为 NULL，`= NULL` 永不命中 → 顶层项完全不去重。
      findChildByKey: this.db.prepare(`SELECT ${WORK_ITEM_FIELDS} FROM work_items WHERE parent_id IS ? AND dedup_key = ? LIMIT 1`),
      // 语义折叠候选：同一父项下所有未完成的"无文件名"项（t: 键）。
      // 排除 done：把新任务折进一个已完成项等于静默丢活；宁可多建一项也不能吞掉真活。
      listTextKeySiblings: this.db.prepare(
        `SELECT id, title, dedup_key FROM work_items
         WHERE parent_id IS ? AND status != 'done' AND dedup_key LIKE 't:%'
         ORDER BY created_at ASC`
      ),
      // 全局去重（跨父项）：dedupKey 抽出的文件路径是完整相对路径，天然全局唯一。
      // 不同父项下的 N 个 worker 各自"创建 src/m1.js"时，若只看同父项就会各自建项 →
      // N 个 worker 同时改同一个文件（用户观察到的"改到一起去了"）。只匹配未完成项：
      // 已 done 的文件不折叠，保留"对既有文件做后续改进"的合法任务。
      // 优先返回 pending（可立即认领），避免把 submitted（门禁中，认领必 409）排在前面。
      findOpenByKeyGlobal: this.db.prepare(
        `SELECT ${WORK_ITEM_FIELDS} FROM work_items WHERE dedup_key = ? AND status != 'done'
         ORDER BY CASE status WHEN 'pending' THEN 0 ELSE 1 END, created_at ASC LIMIT 1`
      ),
      listItems: this.db.prepare(`SELECT ${WORK_ITEM_FIELDS} FROM work_items ORDER BY priority DESC, created_at ASC`),
      listItemsPaged: this.db.prepare(`SELECT ${WORK_ITEM_FIELDS} FROM work_items ORDER BY priority DESC, created_at ASC LIMIT ? OFFSET ?`),
      listItemsByStatusPaged: this.db.prepare(`SELECT ${WORK_ITEM_FIELDS} FROM work_items WHERE status = ? ORDER BY priority DESC, created_at ASC LIMIT ? OFFSET ?`),
      // 思维导图用：只取建树必需列，全量拉取（1000 item 规模下每行 ~60B，开销可忽略）
      listItemTree: this.db.prepare(
        'SELECT id, parent_id, title, kind, priority, status, created_at FROM work_items ORDER BY priority DESC, created_at ASC'
      ),
      countItems: this.db.prepare('SELECT COUNT(*) AS n FROM work_items'),
      countItemsByStatus: this.db.prepare('SELECT COUNT(*) AS n FROM work_items WHERE status = ?'),
      countByStatus: this.db.prepare('SELECT status, COUNT(*) AS n FROM work_items GROUP BY status'),
      updateItemStatus: this.db.prepare('UPDATE work_items SET status = ?, done_at = ? WHERE id = ?'),
      insertClaim: this.db.prepare(
        'INSERT INTO claims (item_id, peer, token, state, acquired_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)'
      ),
      activeClaim: this.db.prepare(
        "SELECT * FROM claims WHERE item_id = ? AND state = 'active' ORDER BY acquired_at DESC"
      ),
      claimByToken: this.db.prepare('SELECT * FROM claims WHERE token = ?'),
      expireClaim: this.db.prepare("UPDATE claims SET state = 'expired' WHERE token = ?"),
      renewClaim: this.db.prepare('UPDATE claims SET expires_at = ? WHERE token = ? AND state = \'active\''),
      releaseClaim: this.db.prepare("UPDATE claims SET state = 'released' WHERE token = ?"),
      insertContext: this.db.prepare(
        `INSERT INTO context (${CONTEXT_FIELDS}) VALUES (@id, @ts, @peer, @kind, @item_id, @text, @tags)`
      ),
      searchContext: this.db.prepare(
        'SELECT * FROM context WHERE text LIKE ? ESCAPE \'\\\' ORDER BY ts DESC LIMIT ?'
      ),
      // 某 item 关联的历史 context（认领时捎带，论文"附带推送"：FAIL 教训直接到手）
      contextByItem: this.db.prepare(
        'SELECT peer, kind, text, ts FROM context WHERE item_id = ? ORDER BY ts ASC LIMIT ?'
      ),
      insertMessage: this.db.prepare(
        'INSERT INTO messages (id, ts, from_peer, to_peer, item_id, text) VALUES (?, ?, ?, ?, ?, ?)'
      ),
      messagesFor: this.db.prepare(
        'SELECT * FROM messages WHERE (to_peer = ? OR to_peer = \'*\') AND consumed_by IS NULL ORDER BY ts'
      ),
      markConsumed: this.db.prepare(
        'UPDATE messages SET consumed_by = ? WHERE id = ? AND consumed_by IS NULL'
      ),
      insertIntegration: this.db.prepare(
        'INSERT INTO integrations (id, ts, peer, item_id, commit_sha, test_cmd, test_passed, merged) VALUES (?, ?, ?, ?, ?, ?, ?, ?)'
      ),
      listIntegrations: this.db.prepare('SELECT * FROM integrations ORDER BY ts DESC'),
      // 监控面板每 2s 轮询一次，只取最近 N 条，避免全表加载（1000 item 规模下集成记录可能上千条）
      recentIntegrations: this.db.prepare('SELECT * FROM integrations ORDER BY ts DESC LIMIT ?'),
      insertSubmission: this.db.prepare(
        'INSERT INTO submissions (id, ts, peer, item_id, branch, commit_sha, state, reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?)'
      ),
      submissionById: this.db.prepare('SELECT * FROM submissions WHERE id = ?'),
      listQueuedSubmissions: this.db.prepare("SELECT * FROM submissions WHERE state = 'queued' ORDER BY ts ASC"),
      updateSubmissionState: this.db.prepare('UPDATE submissions SET state = ?, reason = ? WHERE id = ?'),
      // 空转抑制用：当前"真正抢得到"的工作项数。条件与 tryClaim 的成功路径严格对齐
      // （pending + 无子项 + 未在退避窗口 + 无未过期活跃租约），否则 worker 会误判有活可干而白烧一个 LLM 回合。
      claimableCount: this.db.prepare(
        `SELECT COUNT(*) AS n FROM work_items w
         WHERE w.status = 'pending'
           AND (w.not_before IS NULL OR w.not_before <= ?)
           AND NOT EXISTS (SELECT 1 FROM work_items c WHERE c.parent_id = w.id)
           AND NOT EXISTS (SELECT 1 FROM claims cl WHERE cl.item_id = w.id AND cl.state = 'active' AND cl.expires_at > ?)`
      ),
    };
    this._replay(); // 从事件流重建内存视图（崩溃恢复）
  }

  // node:sqlite 没有 .transaction() 便捷方法，手动 BEGIN IMMEDIATE / COMMIT。
  // 可重入：已在事务内时直接执行 fn（避免嵌套 BEGIN 报错）。
  _tx(fn) {
    if (this._inTx) return fn();
    this._inTx = true;
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const r = fn();
      this.db.exec('COMMIT');
      return r;
    } catch (e) {
      try { this.db.exec('ROLLBACK'); } catch {}
      throw e;
    } finally {
      this._inTx = false;
    }
  }

  _migrate() {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts REAL NOT NULL,
        peer TEXT NOT NULL,
        type TEXT NOT NULL,
        payload TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS work_items (
        id TEXT PRIMARY KEY,
        parent_id TEXT,
        title TEXT NOT NULL,
        detail TEXT DEFAULT '',
        kind TEXT DEFAULT 'task',
        priority INTEGER DEFAULT 0,
        status TEXT DEFAULT 'pending',
        created_by TEXT NOT NULL,
        created_at REAL NOT NULL,
        done_at REAL,
        not_before REAL,
        dedup_key TEXT
      );
      CREATE TABLE IF NOT EXISTS claims (
        token TEXT PRIMARY KEY,
        item_id TEXT NOT NULL,
        peer TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'active',
        acquired_at REAL NOT NULL,
        expires_at REAL NOT NULL
      );
      CREATE TABLE IF NOT EXISTS context (
        id TEXT PRIMARY KEY,
        ts REAL NOT NULL,
        peer TEXT NOT NULL,
        kind TEXT NOT NULL,
        item_id TEXT,
        text TEXT NOT NULL,
        tags TEXT DEFAULT ''
      );
      CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        ts REAL NOT NULL,
        from_peer TEXT NOT NULL,
        to_peer TEXT NOT NULL,
        item_id TEXT,
        text TEXT NOT NULL,
        consumed_by TEXT
      );
      CREATE TABLE IF NOT EXISTS integrations (
        id TEXT PRIMARY KEY,
        ts REAL NOT NULL,
        peer TEXT NOT NULL,
        item_id TEXT,
        commit_sha TEXT,
        test_cmd TEXT,
        test_passed INTEGER DEFAULT 0,
        merged INTEGER DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS submissions (
        id TEXT PRIMARY KEY,
        ts REAL NOT NULL,
        peer TEXT NOT NULL,
        item_id TEXT NOT NULL,
        branch TEXT,
        commit_sha TEXT,
        state TEXT NOT NULL DEFAULT 'queued',
        reason TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_ctx_text ON context(text);
      CREATE INDEX IF NOT EXISTS idx_ctx_item ON context(item_id);
      CREATE INDEX IF NOT EXISTS idx_msg_to ON messages(to_peer, consumed_by);
      CREATE INDEX IF NOT EXISTS idx_claims_item ON claims(item_id, state);
      CREATE INDEX IF NOT EXISTS idx_sub_state ON submissions(state, ts);
    `);
    // 老库补列：not_before（失败退避用）。SQLite 的 ADD COLUMN 不支持 IF NOT EXISTS，
    // 已存在会抛错，忽略即可。
    try { this.db.exec('ALTER TABLE work_items ADD COLUMN not_before REAL'); } catch { /* 列已存在 */ }
    // 老库补列：dedup_key（拆解幂等归一化键）。加列后回填一次历史行，否则老库里的
    // 已有子项 dedup_key 为空，新子项查不到它们 → 幂等对老数据失效。
    let addedDedupCol = false;
    try { this.db.exec('ALTER TABLE work_items ADD COLUMN dedup_key TEXT'); addedDedupCol = true; } catch { /* 列已存在 */ }
    if (addedDedupCol) {
      const rows = this.db.prepare('SELECT id, title FROM work_items').all();
      const upd = this.db.prepare('UPDATE work_items SET dedup_key = ? WHERE id = ?');
      for (const r of rows) upd.run(dedupKey(r.title), r.id);
    }
    this.db.exec('CREATE INDEX IF NOT EXISTS idx_item_dedup ON work_items(parent_id, dedup_key)');
    // 跨父项去重查询按 dedup_key 单独过滤（不看 parent_id），需要独立的键索引。
    this.db.exec('CREATE INDEX IF NOT EXISTS idx_item_dedup_global ON work_items(dedup_key, status)');
  }

  // ---------- 事件日志（append-only） ----------
  log(peer, type, payload = {}) {
    const info = this.stmts.insertEvent.run(Date.now(), peer, type, JSON.stringify(payload));
    return Number(info.lastInsertRowid);
  }
  eventsAfter(lastId, limit = 200) {
    return this.stmts.getEventsAfter.all(lastId, limit).map((r) => ({
      ...r, payload: JSON.parse(r.payload),
    }));
  }

  // ---------- 工作项（board） ----------
  // 所有创建路径（HTTP POST /item、seed 播种、worker 拆解）都收敛到这一个入口，
  // 因此 parentId 的归一化只在这里做一次：
  //   - 同时接受 camelCase 与 snake_case（seed JSON 常写 parent_id，旧写法不再被静默丢弃）
  //   - 拒绝自引用（id 与 parentId 相同），避免前端建树时成环
  addItem({ title, detail = '', kind = 'task', priority = 0, parentId = null, parent_id = null, createdBy = 'system', id = null, semanticFold = true }) {
    const id2 = id || `t${crypto.randomBytes(4).toString('hex')}`;
    const parent = parentId || parent_id || null;
    const parent2 = parent && parent !== id2 ? parent : null;
    // 拆解幂等：worker 拆解走的是"无显式 id"的 HTTP 路径（POST /item），
    // 而 seed/CLI/测试都自带 id（作者有意指定的结构，故不折叠）。
    // 去重分三级，前两级用 dedupKey(title)（抽取确定性交付物名）而非精确 title：
    // LLM 的标题变体极多（"创建 src/m1.js" / "创建 src/m1.js (f_1(x)=x+1)"），精确匹配等于没去重。
    //   ① 同父项内（任意 key）：同一父项下已有"同一交付物"的子项时直接返回既有项。
    //   ② 跨父项（仅 f: 文件交付物）：dedupKey 抽出的完整相对路径全局唯一，故同一文件
    //      无论挂在哪个父项下都折叠到同一项。这是"多个 worker 改到同一个文件"的根因防护——
    //      折叠后该文件只剩 1 个 item，tryClaim 原子性保证同一时刻只有 1 个 worker 能动它。
    //      只对 f: 生效：t: 是无文件名项的归一化文本，跨父项折叠会误合并语义不同的任务。
    //   ③ 同父项内（仅 t: 无文件名项）：token 子集折叠，兜住"名字不同但任务差不多"——
    //      "实现登录" 与 "实现用户登录功能" 归一化文本不等，却是同一件事。
    //      只在"一项的 token 全被另一项包含"时折叠（子集，非相似度阈值）：
    //      能折 "实现登录"→"实现用户登录功能"，不会把 "实现登录" 与 "实现注册" 吞成一个。
    //      仅限同父项：跨父项做语义折叠会误合并两个大任务下碰巧同名的独立工作。
    //      semanticFold=false 供 server 层的 LLM 判官路径使用：判官在场时词面折叠不再
    //      抢先合并（词面只是召回手段，定生死的是判官）；判官失败时由 foldSemantic()
    //      显式回退到本逻辑。
    // 动机：容器不变式只挡住了"认领容器"，挡不住"N 个 worker 各自直接 POST 建子项"。
    // EXP4 实测 10 个 worker 把 big 重拆成 91 个重复子项，父项永远等不齐 → 任务卡死。
    const key = dedupKey(title);
    if (!id) {
      // parent2 为 null（根级项）时也要查：`parent_id IS NULL` 是合法的去重目标。
      const dup = this.stmts.findChildByKey.get(parent2, key);
      if (dup) {
        this.log(createdBy, 'item.deduped', { id: dup.id, title, key, parentId: parent2, scope: 'parent' });
        return dup;
      }
      if (key.startsWith('f:')) {
        const g = this.stmts.findOpenByKeyGlobal.get(key);
        if (g) {
          this.log(createdBy, 'item.deduped', { id: g.id, title, key, parentId: parent2, scope: 'global' });
          return g;
        }
      } else if (semanticFold) {
        const near = this._findSemanticSibling(parent2, title);
        if (near) {
          this.log(createdBy, 'item.deduped', {
            id: near.id, title, key, nearKey: near.dedup_key, parentId: parent2, scope: 'semantic',
          });
          return this.stmts.getItem.get(near.id);
        }
      }
    }
    this.stmts.insertItem.run({
      id: id2, parent_id: parent2, title, detail, kind,
      priority, status: 'pending', created_by: createdBy,
      created_at: Date.now(), done_at: null, not_before: null, dedup_key: key,
    });
    this.log(createdBy, 'item.added', { id: id2, title, parentId: parent2 });
    return this.stmts.getItem.get(id2);
  }
  item(id) { return this.stmts.getItem.get(id); }
  items() { return this.stmts.listItems.all(); }

  // ---------- LLM 判官去重的支撑查询（两阶段：廉价召回 → judge 终审） ----------
  // 工业标准模式（CrewAI 0.85 阈值升级 / PD3 / 语义缓存论文均同构）：词法只负责召回候选，
  // 只有 LLM judge 判 duplicate 才合并，宁漏勿错。这里把 server 层（POST /item 判官路径）
  // 需要的查询从 addItem 内联逻辑里抽出来暴露：

  // 级别①+② 的 key 查询：同父项精确 key 命中 + 跨父项 f: 文件键全局命中。
  // 判官流程开始前先查这个——key 命中是"确定性交付物相同"（同一文件路径），
  // 无需打扰 judge，直接折叠（与 addItem 原有行为一致，只是把查询提前给 server 复用）。
  findKeyDuplicate(parentId, title) {
    const key = dedupKey(title);
    const dup = this.stmts.findChildByKey.get(parentId ?? null, key);
    if (dup) return { item: dup, key, scope: 'parent' };
    if (key.startsWith('f:')) {
      const g = this.stmts.findOpenByKeyGlobal.get(key);
      if (g) return { item: g, key, scope: 'global' };
    }
    return null;
  }

  // 判官的候选召回：同父项下所有未完成的 t:（无文件名）项。
  // 只召回 t: 项——f: 项已被 findKeyDuplicate 用确定性文件路径精确处理。
  // 排序按 token 重叠度降序（重叠越多越可能是重复），限制条数让 judge prompt 不爆炸。
  // 200-1000 agent 规模下单父项子项可能很多，limit 是必须的闸门。
  semanticCandidates(parentId, title, { limit = 8 } = {}) {
    const rows = this.stmts.listTextKeySiblings.all(parentId ?? null);
    const mine = titleTokens(title);
    const scored = rows
      .map((r) => {
        const theirs = titleTokens(r.title);
        let overlap = 0;
        for (const t of theirs) if (mine.has(t)) overlap++;
        return { row: r, overlap };
      })
      .filter((x) => x.overlap > 0) // 零重叠的候选不必浪费 judge 的 token
      .sort((a, b) => b.overlap - a.overlap);
    return scored.slice(0, Math.max(1, limit)).map((x) => this.stmts.getItem.get(x.row.id) || x.row);
  }

  // 判官失败/不可用时的词面回退：复用 addItem 第三级的子集折叠，但作为独立入口
  // （server 层判官失败后调用它，语义是"judge 没给结论，按保守词面规则再试一次"）。
  // 注意：调用方应已传 semanticFold:false 走完判官流程，这里做的是事后补救折叠。
  foldSemantic(parentId, title, createdBy = 'system') {
    const near = this._findSemanticSibling(parentId ?? null, title);
    if (near) {
      const key = dedupKey(title);
      this.log(createdBy, 'item.deduped', {
        id: near.id, title, key, nearKey: near.dedup_key, parentId: parentId ?? null, scope: 'semantic-fallback',
      });
      return this.stmts.getItem.get(near.id);
    }
    return null;
  }


  // 同父项内的语义折叠：找已存在的"无文件名项"里，与 title token 集互为子集的那个。
  // 两道保守闸门，避免把泛化短标题吞掉：
  //   ① 小的一侧至少 3 个 token —— 否则 "测试" 会被 "添加单元测试" 吸走（它只含 测/试 两字）；
  //   ② 大的一侧不超过小的 3 倍 —— 否则 "登录" 会折进一个 8 字的详细任务。
  // 命中时优先返回"较长"的那一项作为折叠目标（信息更全），于是短说法并入长说法。
  _findSemanticSibling(parentId, title) {
    const mine = titleTokens(title);
    if (mine.size === 0) return null;
    const rows = this.stmts.listTextKeySiblings.all(parentId);
    let best = null;
    for (const r of rows) {
      const theirs = titleTokens(r.title);
      if (theirs.size === 0) continue;
      const [small, big] = mine.size <= theirs.size ? [mine, theirs] : [theirs, mine];
      const subset = isSubset(small, big);
      if (!subset || small.size < 3 || big.size > small.size * 3) continue;
      if (!best || r.title.length > best.title.length) best = r;
    }
    return best;
  }

  // 分页拉取（board 大时避免全量传输）：返回 { items, total, limit, offset, hasMore }
  itemsPaged({ status = null, limit = 200, offset = 0 } = {}) {
    const lim = Math.max(1, Math.min(Number(limit) || 200, 1000));
    const off = Math.max(0, Number(offset) || 0);
    const items = status
      ? this.stmts.listItemsByStatusPaged.all(status, lim, off)
      : this.stmts.listItemsPaged.all(lim, off);
    const total = status
      ? this.stmts.countItemsByStatus.get(status).n
      : this.stmts.countItems.get().n;
    return { items, total, limit: lim, offset: off, hasMore: off + items.length < total };
  }
  pendingItems() { return this.items().filter((i) => i.status === 'pending'); }

  // 返回扁平节点 + 边（parent_id），前端自建树。parent 指向不存在/自身的节点视为根，
  // 防止脏数据导致成环或孤立子树丢失。
  itemTree() {
    const rows = this.stmts.listItemTree.all();
    const ids = new Set(rows.map((r) => r.id));
    const nodes = rows.map((r) => ({
      id: r.id,
      parentId: r.parent_id && ids.has(r.parent_id) && r.parent_id !== r.id ? r.parent_id : null,
      title: r.title,
      kind: r.kind,
      status: r.status,
    }));
    return { nodes, roots: nodes.filter((n) => !n.parentId).map((n) => n.id) };
  }
  _setStatus(id, status) {
    const doneAt = status === 'done' ? Date.now() : null;
    this.stmts.updateItemStatus.run(status, doneAt, id);
  }

  // ---------- claim / fence 租约 ----------
  // 原子认领：事务内检查 pending + 无 active 租约 + 退避已过。返回 token 或 null。
  tryClaim(itemId, peer, leaseSec = 300) {
    const now = Date.now();
    return this._tx(() => {
      const item = this.stmts.getItem.get(itemId);
      if (!item || item.status !== 'pending') return null;
      // 已有子项的 item 是"容器"，只负责聚合进度，不可再被认领干活。
      // 否则 N 个 worker 会把同一个大 item 各自重拆一遍，产生几十个重复子项，
      // 父项永远等不齐"所有子项 done"→ 整个任务卡死（EXP4 实测 79 个重复子项、600s 未收敛）。
      if (this.stmts.childCount.get(itemId).n > 0) return null;
      // 退避窗口：门禁刚失败的 item 在 not_before 之前不可被认领，避免同一失败被反复重试。
      if (item.not_before && item.not_before > now) return null;
      // 清理过期租约
      for (const c of this.stmts.activeClaim.all(itemId)) {
        if (c.expires_at < now) this.stmts.expireClaim.run(c.token);
        else return null; // 已有活跃租约
      }
      const token = `c${crypto.randomBytes(6).toString('hex')}`;
      this.stmts.insertClaim.run(itemId, peer, token, 'active', now, now + leaseSec * 1000);
      this.log(peer, 'item.claimed', { itemId, token, leaseSec });
      return token;
    });
  }
  // 认领成功后的完整"任务包"：item 详情 + 该 item 的历史 context（FAIL 教训等）+ 租约信息。
  // 论文"捎带推送"：认领即拿到干活所需信息，省掉 /item + /context/search 两次额外请求。
  claimResult(token) {
    const c = this.stmts.claimByToken.get(token);
    if (!c) return null;
    const item = this.stmts.getItem.get(c.item_id);
    return {
      token, itemId: c.item_id, expiresAt: c.expires_at,
      item, history: this.stmts.contextByItem.all(c.item_id, 20),
    };
  }
  // fence 检查：token 对应租约必须 active 且未过期，才允许推进
  checkFence(token) {
    const c = this.stmts.claimByToken.get(token);
    if (!c || c.state !== 'active' || c.expires_at < Date.now()) return null;
    return c;
  }
  renew(token, leaseSec = 300) {
    const r = this.stmts.renewClaim.run(Date.now() + leaseSec * 1000, token);
    return r.changes > 0;
  }
  release(token) { this.stmts.releaseClaim.run(token); }
  activeClaims() { return this.db.prepare("SELECT * FROM claims WHERE state='active' AND expires_at > ?").all(Date.now()); }
  // 某 peer 当前持有的活跃租约（自动续期用）
  activeClaimsByPeer(peer) {
    return this.db.prepare("SELECT * FROM claims WHERE peer = ? AND state = 'active'").all(peer);
  }

  // ---------- 共享上下文（append-only，5 类条目） ----------
  addContext(peer, kind, text, { itemId = null, tags = [] } = {}) {
    const row = {
      id: `x${crypto.randomBytes(5).toString('hex')}`, ts: Date.now(),
      peer, kind, item_id: itemId, text, tags: tags.join(','),
    };
    this.stmts.insertContext.run(row);
    this.log(peer, 'context.added', { kind, itemId, text: text.slice(0, 120) });
    return row.id;
  }
  searchContext(query, limit = 30) {
    const esc = query.replace(/[\\%_]/g, (m) => '\\' + m);
    return this.stmts.searchContext.all(`%${esc}%`, limit);
  }
  // 某时刻之后的新 context 条目（digest / 回合简报用）。时间正序返回，最新贴近上下文末尾。
  contextSince(ts, limit = 12, excludePeer = null) {
    const rows = excludePeer
      ? this.db.prepare('SELECT peer, kind, item_id, text, ts FROM context WHERE ts > ? AND peer != ? ORDER BY ts DESC LIMIT ?').all(ts, excludePeer, limit)
      : this.db.prepare('SELECT peer, kind, item_id, text, ts FROM context WHERE ts > ? ORDER BY ts DESC LIMIT ?').all(ts, limit);
    return rows.reverse();
  }
  // ---------- digest（论文"可见区"）：一次调用拉齐增量，替代 board+messages+context 三连拉 ----------
  // 省 token 的关键：pending 只给 id+title 预览（detail 按需 GET /item），
  // context 只给 since 之后的新条目（更早的靠 /context/search grep）。
  digestFor(peer, sinceTs = 0, limit = 50) {
    const now = Date.now();
    const stats = this.stats();
    const pendingPreview = this.db.prepare(
      "SELECT id, title, priority, kind FROM work_items WHERE status = 'pending' AND (not_before IS NULL OR not_before <= ?) ORDER BY priority DESC, created_at ASC LIMIT ?"
    ).all(now, limit);
    const claims = this.activeClaimsByPeer(peer).map((c) => {
      const item = this.stmts.getItem.get(c.item_id);
      return { token: c.token, itemId: c.item_id, title: item ? item.title : null, expiresAt: c.expires_at };
    });
    const newContext = this.contextSince(Number(sinceTs) || 0, limit);
    const messages = this.consumeMessages(peer); // 读走即消费（与 GET /messages 同语义）
    return { now, stats, pendingPreview, claims, newContext, messages };
  }

  // ---------- 消息 ----------
  postMessage(from, to, text, itemId = null) {
    const id2 = `m${crypto.randomBytes(5).toString('hex')}`;
    this.stmts.insertMessage.run(id2, Date.now(), from, to, itemId, text);
    this.log(from, 'message.posted', { to, itemId });
    return id2;
  }
  consumeMessages(peer) {
    const rows = this.stmts.messagesFor.all(peer);
    for (const r of rows) this.stmts.markConsumed.run(peer, r.id);
    return rows;
  }

  // ---------- 集成记录 ----------
  recordIntegration(peer, itemId, commitSha, testCmd, testPassed, merged) {
    const id2 = `i${crypto.randomBytes(5).toString('hex')}`;
    this.stmts.insertIntegration.run(id2, Date.now(), peer, itemId, commitSha, testCmd, testPassed ? 1 : 0, merged ? 1 : 0);
    this.log(peer, 'integration.recorded', { itemId, commitSha, testPassed, merged });
    return id2;
  }
  integrations() { return this.stmts.listIntegrations.all(); }
  recentIntegrations(limit = 20) { return this.stmts.recentIntegrations.all(Math.max(1, Number(limit) || 20)); }

  // ---------- 派生视图：完成工作项 ----------
  // agent 提交成功且集成通过后调用；也用于子项全部 done 时自动 done 父项
  completeItem(itemId, peer, commitSha) {
    return this._tx(() => {
      const item = this.stmts.getItem.get(itemId);
      if (!item) return false;
      this._setStatus(itemId, 'done');
      this.log(peer, 'item.done', { itemId, commitSha });
      // 父项级联：所有兄弟子项 done → 父项 done
      if (item.parent_id) {
        const siblings = this.db.prepare('SELECT status FROM work_items WHERE parent_id = ?').all(item.parent_id);
        if (siblings.length > 0 && siblings.every((s) => s.status === 'done')) {
          this._setStatus(item.parent_id, 'done');
          this.log(peer, 'item.done.auto', { itemId: item.parent_id });
        }
      }
      return true;
    });
  }

  // ---------- 提交 → 集成门禁 → done ----------
  // worker 调 /submit 只是"交作业"：校验租约、把 item 标为 submitted（不可再被认领）、
  // 入队一条 submission。真正的 done 由集成器跑完门禁、合并成功后调 completeItem 决定。
  // 这样门禁失败时 item 会退回 pending，别人可重试。
  submitWork(token, peer, commitSha = null) {
    const fence = this.checkFence(token);
    if (!fence) return null;
    const itemId = fence.item_id;
    const r = this._tx(() => {
      const item = this.stmts.getItem.get(itemId);
      if (!item || item.status !== 'pending') return null;
      this._setStatus(itemId, 'submitted');
      const sid = `s${crypto.randomBytes(5).toString('hex')}`;
      this.stmts.insertSubmission.run(sid, Date.now(), peer, itemId, `swarm/w-${peer}`, commitSha, 'queued', null);
      this.stmts.releaseClaim.run(token);
      this.log(peer, 'item.submitted', { itemId, submissionId: sid, commitSha });
      return { submissionId: sid, itemId };
    });
    // 事务提交后再唤醒集成器（事件驱动）：消除"最后一次提交 → 开始门禁"的固定轮询延迟。
    // 只在真的入队成功时通知（r 非空），失败路径不触发。
    if (r) { try { this._onSubmit?.(); } catch { /* 监听器异常不影响提交 */ } }
    return r;
  }
  queuedSubmissions() { return this.stmts.listQueuedSubmissions.all(); }
  // reaper 用：该 peer 是否还有未集成的提交（有则不能回收其 worktree/分支，否则集成器 merge 找不到分支）
  hasQueuedSubmissionForPeer(peer) {
    return this.db.prepare("SELECT COUNT(*) AS n FROM submissions WHERE peer = ? AND state = 'queued'").get(peer).n > 0;
  }
  // 门禁结果落库：merged → item done；rejected → item 退回 pending 供重试
  resolveSubmission(submissionId, ok, reason = null) {
    return this._tx(() => {
      const s = this.stmts.submissionById.get(submissionId);
      if (!s) return false;
      this.stmts.updateSubmissionState.run(ok ? 'merged' : 'rejected', reason, submissionId);
      if (ok) {
        this.completeItem(s.item_id, s.peer, s.commit_sha);
      } else {
        const item = this.stmts.getItem.get(s.item_id);
        if (item && item.status === 'submitted') {
          // 指数退避：同一 item 连续失败越多次，退避越久，避免弱模型陷入"失败→立即重试"死循环。
          const fails = this.db.prepare(
            "SELECT COUNT(*) AS n FROM submissions WHERE item_id = ? AND state = 'rejected'"
          ).get(s.item_id).n;
          const backoffMs = Math.min(60000, 5000 * Math.pow(2, Math.min(fails, 4)));
          this.db.prepare('UPDATE work_items SET status = ?, not_before = ? WHERE id = ?')
            .run('pending', Date.now() + backoffMs, s.item_id);
          this.log(s.peer, 'item.retry_backoff', { itemId: s.item_id, fails, backoffMs });
        }
      }
      this.log(s.peer, 'submission.resolved', { submissionId, ok, reason });
      return true;
    });
  }

  // ---------- 回放（崩溃恢复用） ----------
  // 事件流是 append-only 的审计线。这里把"会影响 work_items.status 的事件"按 id 顺序折叠，
  // 得到每个工作项的应然状态，再与派生表（work_items.status）比对：二者本由同一事务写入，
  // 正常必然一致；不一致即崩溃截断或手工改库留下的漂移。
  // 修复策略刻意保守——只做单向、不会丢工作的修正（事件说 done 而表里没 done → 补 done），
  // 绝不把 done 退回 pending（那会把已完成的活重跑一遍）。其余方向的漂移只统计、不修改。
  _replay() {
    const STATUS_OF_EVENT = {
      'item.added': 'pending',
      'item.submitted': 'submitted',
      'item.retry_backoff': 'pending',
      'item.done': 'done',
      'item.done.auto': 'done',
    };
    const expect = new Map();
    try {
      const rows = this.db.prepare(
        "SELECT type, payload FROM events WHERE type IN ('item.added','item.submitted','item.retry_backoff','item.done','item.done.auto') ORDER BY id"
      ).all();
      for (const r of rows) {
        let p; try { p = JSON.parse(r.payload); } catch { continue; }
        if (!p || !p.itemId) continue;
        expect.set(p.itemId, STATUS_OF_EVENT[r.type]); // 同一项多条事件：后写的覆盖先写的
      }
    } catch { this.lastReplay = { scanned: 0, repaired: 0, mismatches: 0 }; return; }
    let repaired = 0, mismatches = 0;
    for (const [id, want] of expect) {
      const item = this.stmts.getItem.get(id);
      if (!item || item.status === want) continue;
      mismatches += 1;
      if (want === 'done') { this._setStatus(id, 'done'); repaired += 1; }
    }
    this.lastReplay = { scanned: expect.size, repaired, mismatches };
    if (repaired > 0) this.log('store', 'store.replayed', { ...this.lastReplay });
  }

  // 崩溃恢复（显式调用，只在 run 启动、尚无 worker 存活时）：把上一次进程崩溃留下的
  // active 租约全部作废。不清理的话，对应工作项要等租约自然过期（默认 300s）才能被重新认领。
  // 刻意不放进构造函数：swarm status / board 会打开同一个库，那样会误杀正在跑的租约。
  releaseOrphanClaims() {
    const n = this.db.prepare("UPDATE claims SET state = 'expired' WHERE state = 'active'").run().changes;
    if (n > 0) this.log('store', 'recover.orphan_claims', { count: n });
    return n;
  }

  stats() {
    // 用 SQL COUNT/GROUP BY，避免全表加载后在 JS 里 filter（1000 item 规模下 autoscaler 每 5s 调一次）
    const rows = this.stmts.countByStatus.all();
    const by = {};
    let total = 0;
    for (const r of rows) { by[r.status] = r.n; total += r.n; }
    return {
      total, pending: by.pending || 0, submitted: by.submitted || 0,
      done: by.done || 0, inProgress: by.in_progress || 0,
    };
  }

  // ---------- autoscaler 的深度信号 ----------
  // 单一大任务场景下，pending 只有 1~2 个（大 item 靠 worker 自己拆子项），广度信号严重不足，
  // 永远停在 minWorkers。所以补两个"深度信号"：
  //   openCount：所有尚未完成的工作项，含被认领的（claim 不改 status）和已提交、正在等门禁的
  //             （status='submitted'）。每一项都可能还需要一个 worker。
  //             注意必须计入 submitted：门禁跑完前它仍是"活"，否则集成器一跑门禁 openCount 就掉到 0，
  //             导致 worker 误判无活可干而提前退场、autoscaler 误缩容。
  //   recentSplitCount：最近拆解出的子项数——工作正在展开的领先指标。
  //             拆解是 swarm 扩大并行度的唯一机制，这个信号让 autoscaler 在子项被认领前就先加人。
  openCount() {
    return this.db.prepare(
      "SELECT COUNT(*) AS n FROM work_items WHERE status IN ('pending','in_progress','submitted')"
    ).get().n;
  }
  recentSplitCount(windowMs = 300000) {
    return this.db.prepare(
      'SELECT COUNT(*) AS n FROM work_items WHERE parent_id IS NOT NULL AND created_at > ?'
    ).get(Date.now() - windowMs).n;
  }
  // 当前"真正抢得到"的工作项数（worker 空转抑制用）。与 tryClaim 的成功条件严格对齐：
  // pending + 无子项（容器不可认领）+ 未在退避窗口 + 无未过期活跃租约。
  claimableCount() {
    const now = Date.now();
    return this.stmts.claimableCount.get(now, now).n;
  }
  close() { this.db.close(); }
}
