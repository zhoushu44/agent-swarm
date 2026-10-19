// AIMD 动态天花板测试：503 动态探测的核心场景，纯内存离线可跑。
// 覆盖回归：撞限流后天花板钉在真实水位（不再反弹回旧顶格反复撞）+ 成功后缓慢上探。
import assert from 'node:assert';
import { Semaphore } from '../src/harness.js';

function check(name, fn) {
  try { fn(); console.log(`  ok  ${name}`); }
  catch (e) { console.error(`  FAIL ${name}: ${e.message}`); throw e; }
}

export async function run() {
  console.log('AIMD 动态天花板（503 动态探测）');

  check('基线：初始 limit=ceiling=64', () => {
    const s = new Semaphore(64);
    assert.equal(s.limit, 64);
    assert.equal(s.ceiling, 64);
  });

  check('撞限回退：ceiling 钉在撞限水位，limit 退到容量之下', () => {
    const s = new Semaphore(64);
    // 模拟真实：并发爬到 20 时中转站开始 503（hiWater=20）
    s._hiWater = 20; s.active = 20;
    s.noteRateLimit();
    assert.equal(s.ceiling, 20, 'ceiling 应钉在实际并发水位 20，而不是留在 64');
    assert.equal(s.limit, 15, 'limit 应退到 floor(min(64,20)*0.75)=15（容量下卸压）');
  });

  check('回退后回升不会反弹回旧天花板 64（收敛在 20 附近）', async () => {
    const s = new Semaphore(64);
    s._hiWater = 20; s.active = 20;
    s.noteRateLimit(); // ceiling=20, limit=15
    // 全部成功，连续 noteOk 推着 limit 回升：每次顶到 ceiling 时 ceiling 只 +1
    // 200 次 ok → 25 次加性回升，ceiling ≤ 20+25=45 < 64
    for (let i = 0; i < 200; i++) s.noteOk();
    assert.ok(s.ceiling <= 45, `ceiling=${s.ceiling} 应收敛在远低于旧顶格 64 的水位`);
    assert.ok(s.limit <= s.ceiling, `limit=${s.limit} 不应超过动态 ceiling=${s.ceiling}`);
  });

  check('连续 8 次成功 → limit +1（加性回升）', () => {
    const s = new Semaphore(10);
    for (let i = 0; i < 8; i++) s.noteOk();
    assert.equal(s.limit, 11, '8 次成功后 limit 应 +1');
    assert.equal(s.ceiling, 11, '顶到上限成功时 ceiling 跟随上探');
  });

  check('服务器恢复后动态提升：顶格持续成功 → ceiling 加速上探不永久钉死', () => {
    const s = new Semaphore(64);
    s._hiWater = 20; s.active = 20;
    s.noteRateLimit(); // ceiling=20, limit=15（钉顶）
    // 服务器扩容了：持续成功 500 次（streak 不断档）
    for (let i = 0; i < 500; i++) s.noteOk();
    // streak≥256 → probe=4：ceiling 每轮 +4 爬升，500 次 ok ≈ 62 轮 → 应远超 20
    assert.ok(s.ceiling > 40, `ceiling=${s.ceiling} 应动态爬升远超撞限水位 20（服务器恢复不被钉死）`);
    assert.ok(s.limit >= 20, `limit=${s.limit} 应随健康度回升到撞限水位之上`);
  });

  check('503 风暴节流：同窗内多个 503 只回退一次，不打塌到 1', () => {
    const s = new Semaphore(64);
    s._hiWater = 20; s.active = 20;
    s.noteRateLimit(); // 第一次：ceiling=20, limit=15
    // 同一风暴窗内（<2s）并发涌来 30 个 503
    for (let i = 0; i < 30; i++) { s._hiWater = 20; s.noteRateLimit(); }
    assert.equal(s.limit, 15, `limit=${s.limit} 风暴窗内只应回退一次（15），不应被打塌`);
    assert.equal(s.ceiling, 20, `ceiling=${s.ceiling} 不应被风暴连击下修`);
  });

  check('二次撞限（冷却窗外）：ceiling 进一步下修到更低水位', () => {
    const s = new Semaphore(64);
    s._hiWater = 20; s.active = 20; s.noteRateLimit(); // ceiling=20, limit=15
    // 模拟冷却窗已过（回退后回升又撞限在更低水位）
    s._lastBackoffAt = 0; s._hiWater = 12; s.active = 12;
    s.noteRateLimit();
    assert.equal(s.ceiling, 12, '二次撞限应把 ceiling 下修到 12');
    assert.equal(s.limit, 9, 'limit 应退到 floor(min(15,12)*0.75)=9');
  });

  check('active=0 时撞限：ceiling 至少为 1（不塌到 0）', () => {
    const s = new Semaphore(64);
    s.active = 0; s._hiWater = 0;
    s.noteRateLimit();
    assert.ok(s.ceiling >= 1, `ceiling=${s.ceiling} 不应塌到 0`);
    assert.ok(s.limit >= 1, `limit=${s.limit} 不应塌到 0`);
  });

  check('极限收敛：反复在低水位撞限 → limit 单调下降但 ≥1', () => {
    const s = new Semaphore(64);
    for (let i = 0; i < 40; i++) { s.active = 1; s._hiWater = 1; s.noteRateLimit(); }
    assert.equal(s.limit, 1, '反复撞限最终收敛到 1');
    assert.equal(s.ceiling, 1);
  });

  check('真实异步路径：acquire/release 交替，撞限后真实水位收敛', async () => {
    const s = new Semaphore(32);
    // 场景：中转站容量 8——第 8 个并发之后全部 503。
    // worker 模式：acquire → (模拟请求) → 成功 noteOk+release / 失败 noteRateLimit+release
    let inflight = 0;
    const round = async () => {
      await s.acquire();
      inflight += 1;
      // 服务端容量 8：超过即 503
      const ok = inflight <= 8;
      // 留一点时间让并发真堆上去
      await new Promise((r) => setTimeout(r, 2));
      inflight -= 1;
      if (ok) s.noteOk(); else s.noteRateLimit();
      s.release();
    };
    // 三波并发冲刺（每波 32 个并发请求）。波间隔 >2s 跨过风暴节流冷却窗，
    // 每波独立学习；波内 <2s 的并发 503 只回退一次（风暴节流生效）。
    for (let wave = 0; wave < 3; wave++) {
      await Promise.all(Array.from({ length: 32 }, () => round()));
      await new Promise((r) => setTimeout(r, 2100));
    }
    // 收敛断言：limit 学到了容量约束（显著低于初始 32），且 ≥1。
    // 钉顶水位（ceiling）= 过载时刻的瞬时峰值并发，天然略高于真实容量
    // （503 响应有网络延迟，期间新请求还在涌入）——不断言精确等于 8，
    // 只断言学到了约束且没被风暴打塌、也没被波间成功推回顶格。
    assert.ok(s.limit < 32, `limit=${s.limit} 应已从 32 收敛下降`);
    assert.ok(s.limit >= 1, `limit=${s.limit} 不应塌到 0`);
    assert.ok(s.ceiling < 32, `ceiling=${s.ceiling} 应低于初始顶格 32（学到容量约束）`);
  });

  console.log('AIMD 动态天花板：全部通过');
}
