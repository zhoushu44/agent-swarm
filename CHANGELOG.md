# Changelog

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
