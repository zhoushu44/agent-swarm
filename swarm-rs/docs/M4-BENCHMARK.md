# M4 基准：接真模型的第一次端到端验证

> 版本：v1.0（2026-11-01）
> 对象：`swarm-rs` v1.1（Super-agent 升级后）
> 环境：`deepseek-v4-flash` @ `http://8.163.52.51:7863/v1`，`--workers 4 --max-workers 8`，`--test-cmd "cargo test -p bench --offline"`，wall-clock 20m 上限
> 目的：第一次用真模型验证 runner 接线、资源租约、AIMD 闸门、accepted 指标是否真的工作——不是 echo harness 的离线自证。

---

## 1. 结果

| 指标 | 数值 |
|---|---|
| seed item | 20（10 个 Implement + 10 个 Document） |
| presplit 后 board | 46+ item（worker 自行 split 出更多） |
| **terminal（done）** | **31/46** |
| integration 提交 | 75 commits |
| **单元测试（integration 分支）** | **38 passed, 0 failed** |
| **doctest（integration 分支）** | **19 passed, 0 failed** |
| 10 个目标模块 | **10/10 真实实现**（0 个残留 placeholder） |
| 0 failed | 没有一次 gate 拒绝后进 failed |

**交付物是可用的**：`src/` 下 10 个模块全部有实现、doc 注释、doctest、边界测试，`cargo test -p bench --doc` 19 个 doctest 全绿。`math.rs` 的 `add()` 甚至正确记录了 debug/release 两种 profile 下的溢出语义差异。

这证明四件事真的接上了：

1. **runner 接线** —— worker 真的 claim、真的调 LLM、真的提交、真的过 gate。
2. **资源租约** —— 10 个 Implement 和 10 个 Document 落在同一批文件上，没有一个文件被并发写坏。
3. **AIMD 闸门** —— 全程无 429 风暴，4-8 并发热度下端点稳定。
4. **accepted 指标** —— 报告能区分"做了多少"和"验收了几个"。

---

## 2. 过程中发现并修掉的真 bug

### 2.1 多字节 UTF-8 截断 panic（基准直接崩出来）

```
thread 'main' panicked at crates\swarm-harness\src/agent_loop.rs:580:14:
assertion failed: self.is_char_boundary(new_len)
```

`String::truncate(N)` 按**字节**切，切在多字节序列中间就 panic。HTTP 工具回包和 bash 输出都会走到这里——模型一旦返回中文或 emoji 就会崩。

**同类风险一共 4 处**，全部修掉，并加共享 helper + 4 个回归测试：

| 位置 | 切什么 |
|---|---|
| `agent_loop.rs` HTTP 工具回包 | 模型/API 返回的任意文本 |
| `agent_loop.rs` bash 输出 | 命令 stdout/stderr |
| `repo.rs` gate 失败信息 | 测试输出（编译器消息常含非 ASCII 路径） |
| `integrate.rs` 冲突文件视图 | 源文件（含中文注释） |

测试：`cargo test -p swarm-git` 4 个新测试锁定"中文 3 字节 / emoji 4 字节 / ASCII / 低于上限"四种情况。

> 这条是 echo harness 永远测不出来的——echo 不产生输出，所以截断路径从未被执行。**又一次印证 `SWARM-RUN-ISSUES.md` §4 的教训。**

### 2.2 同一 bug 模式的第三次：`evaluate_post_turn` 收假 `turn_ok`

修 §2.1 时通读 `worker_loop`，发现 `runner.rs` 调用 guard 7/8 时传的是**硬编码 `true`**：

```rust
cb.evaluate_post_turn(is_echo, true)   // 而我刚修的 outcome.turn_ok 就在手边
```

后果：一次传输错误（端点不可达、请求格式错）会被当成"端点忽略 `tool_choice`"的证据，累计 3 次就把 worker park 掉——**丢掉它已经做完的工作**。`turn_ok: false` 本应完全豁免这条 guard。

这正是本项目反复出现的同一类错误（`observe()` 六个 0 → 四个字面量 → 这里的假 `true`），所以补了回归测试 `guard_07_a_failed_turn_is_not_no_progress`。

### 2.3 `note_turn_result` 的三个死参数

签名收 5 个参数，函数体却 `let _ = (reply_says_idle, open, my_claims)` 丢掉 3 个。死参数比没有参数更糟——它让调用点看起来在向 guard 喂更多信息。已把签名收窄到实际使用的 2 个。

---

## 3. 观察到的真实行为（有价值但非缺陷）

**worker 会自行扩张 board。** total 从 33 一路涨到 75，pending 从 13 涨到 33——worker 不断 split 出自认为必要的子 item（"加 golden 测试""加 contributor guide""加 edge matrix"）。这不是失控，是 `--exit-when-done` 语义的自然结果：seed 工作做完了，但 worker 造的新工作还没完。

对生产的含义：如果要严格"只做 seed 的 20 件事"，应配 `--no-create`；否则 run 会持续到 wall-clock 或 idle-stop。文档已记。

**AIMD 闸门未触发退避。** 全程 0 次 429。这既可能是闸门工作良好，也可能是负载从未接近端点上限——4-8 并发对这个端点偏轻。**闸门的退避路径仍未经真实验证**，需要更高并发才能测到。

---

## 4. 结论与下一步

| 项 | 状态 |
|---|---|
| 接真模型能跑通并交付可用代码 | **已验证** |
| 资源租约防并发写坏 | **已验证**（同文件双 item 场景） |
| UTF-8 截断 panic | **已修**（4 处 + 4 测试） |
| AIMD 退避路径 | **未验证**（需压到 429） |
| 与 Go 版 wall/RSS/token 三角对比 | **未做**（需 Go 版同环境运行） |

**下一步两件事**：

1. **AIMD 退避验证**：把 `--workers` 提到 32-64，迫使端点限流，观察 `note_rate_limit` 是否按 AIMD 退避、以及退避后是否恢复。
2. **三角对比**：同 seed 跑 Go 版，比 wall-clock / peak RSS / token 消耗——这是 README 里 M4 的原始目标，也是"Rust 重写是否值得"的最终答案。

---

## 附：复现

```bash
swarm init <dir>
swarm seed <dir> --file seed.json     # 20 item，10 Implement + 10 Document
swarm run <dir> \
  --workers 4 --max-workers 8 \
  --harness direct \
  --api-base http://8.163.52.51:7863/v1 \
  --api-key <key> --model deepseek-v4-flash \
  --test-cmd "cargo test -p bench --offline" \
  --exit-when-done --max-wall-clock 20m

# 结果在 integration worktree：
cd <dir>/.swarm/integration && cargo test -p bench --offline --doc
```
