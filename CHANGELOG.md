# Changelog

## 3.1.0

主题：**AIMD 闸门 503 恢复语义完善**——服务器从过载恢复后吞吐能自动爬回，不被永久钉在撞限瞬间的低位。

### 修正：src/harness.js Semaphore 三处

- **503 风暴节流**：`noteRateLimit` 加 2s 冷却窗。同一次过载风暴里多个并发请求几乎同时 503 时，只让第一个做"钉顶 + 回退"，其余在冷却窗内直接跳过——否则 N 个并发 503 会连续乘性回退把 limit 打塌到 1，恢复极慢。
- **hiWater 峰值消费后重置**：原实现只在 `hitLevel < ceiling` 分支才清零 `_hiWater`，撞在更高水位时旧峰值残留，导致后续撞限学不到更低水位。现统一在钉顶后重置，下一轮重新累积真实峰值。
- **恢复上探拆为两条路径**：
  - 容量内回升：`limit < ceiling` 时每 8 次 OK 加性 +1（AIMD 的 AI），不越过钉顶水位——撞限后的保守恢复期。
  - 天花板动态上探：`limit >= ceiling` 且持续成功时，按 streak 分档放大步长（≥64 → +2，≥256 → +4）继续上探 ceiling。服务器扩容/恢复后吞吐能较快爬回甚至超越撞限前水平；一旦再撞限 streak 立即归零回到最保守 +1。

### 测试

- `test/aimd_test.js` 同步更新断言，7/7 通过。

### 关联版本

Go 重写版（`swarm-go/` 仓库）同步落地同一套 AIMD 语义，作为 **v4.0.0** 单独发布——单二进制零依赖，cmdRun 组装层（worker 池 / autoscaler / integrator / renewer / reaper / sysload 护栏 / AIMD 闸门）全量接通，`go test ./...` 全绿。详见 swarm-go 仓库的 CHANGELOG。

## 3.0.0

主题：**503 降速后能动态提升回来**（服务器恢复不被永久钉在低位）。

### 变更：AIMD 自适应闸门

- `src/harness.js` 的 apiGate 改为 AIMD（加性增/乘性减）：
  - 撞 429/503 时把"峰值并发水位"钉为新 ceiling（钉住的是撞限瞬间的真实容量），再 0.75x 乘性回退到其下
  - 撞限时并发统计用 `hiWater`（本轮峰值），避免被先 Release 的请求低估撞限水位
  - 持续成功时每 8 次 OK 加性 +1 回升到 ceiling；顶格后按成功 streak 分档上探（≥64 → +2，≥256 → +4），服务器恢复后自动爬回去
  - 2s 冷却窗内多个 503 只回退一次（风暴节流，避免 N 个并发失败把 limit 乘到 1）
- Go 版 `internal/harness/adaptive.go` 语义完全对齐，测试 7/7

## 2.0.0

主题：**不再把本机跑死**。一次 12 worker 的实跑把普通 PC 的 CPU 和内存吃干卡死，
这个版本把「并发不受控」从使用习惯问题变成代码层面的硬约束。

### 新增：系统资源护栏

- `src/sysload.js`：CPU 占用用 `os.cpus()` times 差分采样（Windows 的 `loadavg` 恒为 0，不可用），
  配合空闲内存比例，得出 `ok` / `high` / `critical` 三档压力。
- autoscaler 每轮扩容前按压力钳制目标 worker 数：
  - `ok`（CPU < 80% 且内存 > 15%）→ 不设限，尊重显式 `--max-workers`
  - `high`（CPU > 80% 或内存 < 15%）→ 只许停不许扩
  - `critical`（CPU > 92% 或内存 < 8%）→ 主动缩容一个，下限 1
- `status()` 暴露 `resourceGuard`；压力档位变化时发 `autoscaler.guard_pressure` 事件（仅变化时发）。
- `--no-resource-guard` 可关闭护栏（逃生用，不建议长期开）。

### 变更：默认值自适应

- `--max-workers` 缺省从固定 8 改为 `max(2, ⌈核数/2⌉)`（12 核机器 → 6）。
- `--max-inflight` 缺省从 `max(64, workers)` 改为 `clamp(workers × 4, 8, 64)`，
  `--max-inflight` > `SWARM_MAX_INFLIGHT` > 自适应，优先级与文档一致。
- 启动行打印 `inflight<=N` 和 `resource-guard=on/OFF`，一眼核对实际生效值。

### 文档：运行时安全

- `scripts/install-skill.js` 生成的 skill 新增「运行时安全（必须遵守）」章节：
  禁止给 swarm 加外部看门狗保活、必须 `--exit-when-done`、凭据不得进命令行、
  不要绕过资源护栏，并附「电脑被卡死」的三步处置顺序。
- README 新增「系统资源护栏」章节与「电脑被跑卡死 / node 进程杀了又活」FAQ。

### 测试

- 新增 `test/resource_guard_test.js`：纯函数裁决、采样器、autoscaler 集成（注入假探针）共 16 项。
- 既有测试显式关闭护栏，避免宿主机真实负载导致断言偶发失败。
- `node test/run_tests.js` 四个套件全通过。

## 0.1.0

初始版本：去中心化多 agent 编排器，worker 自拉 board、原子认领、集成器串行 merge 加测试门禁。
