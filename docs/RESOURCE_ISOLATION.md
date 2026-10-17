# 资源隔离演进路线

> **现状**：swarm 所有 worker 跑在单个 Node 进程里（async 函数池）。100-200 workers 稳定，500+ 会撞墙。
>
> **结论**：分阶段演进，**不要现在换 Go**。

---

## 为什么单进程 Node 现在够用

### 当前架构（100-200 workers）

```
Runner (单进程)
├── worker 1 (async 函数)
├── worker 2 (async 函数)
├── ...
└── worker 100 (async 函数)
    ↓ 全部共享
  - Node 事件循环（单线程）
  - 堆内存（V8 GC）
  - SQLite 连接（better-sqlite3 同步阻塞）
```

**实测数据**（stress-v2，100 workers）：
- 峰值吞吐：86k tok/s
- 39/39 任务完成，296/296 测试通过
- 无内存泄漏、无事件循环卡顿

**结论**：100-200 workers 规模下，Node 单进程**完全够用**。

---

## 什么时候会撞墙（500+ workers）

### 瓶颈 1：单线程事件循环

**现象**：
- 一个 worker 的同步重计算（大文件 `JSON.parse`、git 输出正则匹配）卡住其他 499 个
- `console.log` 输出拥堵（stdout 是同步的）

**触发条件**：
- worker 数 > 300
- 单个 worker 每回合处理大文件（> 1MB）

### 瓶颈 2：内存泄漏传染

**现象**：
- 一个 worker 的 `fetch` 流没正确释放（harness 里 `res.body` 没读完）
- V8 堆内存膨胀 → GC 频繁 → 全部 worker 变慢 → OOM

**触发条件**：
- LLM 流式响应中断（网络抖动）
- worker 异常退出但流没清理

### 瓶颈 3：SQLite 锁竞争

**现象**：
- better-sqlite3 是同步阻塞，一个 worker 写库时其他 499 个等
- `SQLITE_BUSY` 错误增多

**触发条件**：
- worker 数 > 200
- 高频事件写入（每秒 > 100 条事件）

### 瓶颈 4：子进程爆炸

**现象**：
- 每个 worker 每回合开多个 `git` 子进程（status/merge/add/commit）
- 500 workers × 5 git 命令 = 2500 个进程
- Windows 进程创建成本高（~50ms/个）

**触发条件**：
- worker 数 > 300
- git 操作频繁（每回合 > 3 次）

---

## 演进路线（分三阶段）

### 阶段 1️⃣：现在（100-200 workers）

**策略**：继续优化单进程 Node。

**优化点**：
- ✅ 已完成：KV 前缀缓存（99.1% 跨 worker 共享）
- ✅ 已完成：AIMD 自适应 max-inflight
- ✅ 已完成：SQLite WAL 模式（并发读写）
- 🔲 可选：JSON.parse 换成流式解析（`stream-json`）
- 🔲 可选：git 输出用 `readline` 逐行读（避免一次性加载大输出）

**不要做的事**：
- ❌ 不要现在换 Go（过度工程）
- ❌ 不要现在上 worker_threads（100 workers 用不到）

---

### 阶段 2️⃣：500 workers（worker_threads）

**触发条件**：
- 实测 300+ workers 时事件循环延迟 > 100ms
- 或者 SQLite `SQLITE_BUSY` 错误率 > 5%

**策略**：worker 从 async 函数改成 **worker_threads**。

**架构变化**：

```
Runner (主进程)
├── WorkerThread 1 (独立 V8 isolate + 事件循环)
│   └── 内部跑 50 个 async worker
├── WorkerThread 2
│   └── 50 个 async worker
├── ...
└── WorkerThread 10
    └── 50 个 async worker
```

**关键设计**：
- 每个 WorkerThread 内部还是 async 函数池（50 个 worker 共享一个事件循环）
- WorkerThread 之间通过 `postMessage` 通信（共享 store 句柄、board 快照）
- SQLite 用 WAL 模式 + 连接池（每个 WorkerThread 一个连接）

**工作量**：1-2 天
- 改造 `Runner.spawnWorker()`：从 `new Promise()` 改成 `new Worker()`
- 实现 WorkerThread 间的 store 同步（`SharedArrayBuffer` 或消息传递）
- 崩溃隔离：WorkerThread 崩溃只影响自己的 50 个 worker

**能撑到**：500-800 workers

---

### 阶段 3️⃣：1000+ workers（Go 重写）

**触发条件**：
- worker_threads 撑到 800 workers 后撞墙（IPC 通信成本、SharedArrayBuffer 序列化开销）
- 或者需要跨机器分布式（多台服务器跑同一个 swarm）

**策略**：用 Go 重写核心运行时。

**为什么 Go 是对的**：

| 维度 | Node worker_threads | Go |
|---|---|---|
| 并发模型 | 每线程一个事件循环，线程间消息传递 | goroutine 真并发，共享内存 |
| 内存隔离 | WorkerThread 独立堆，但 IPC 成本高 | goroutine 共享堆，GC 可控 |
| 崩溃隔离 | WorkerThread 崩溃影响 50 workers | goroutine panic 只影响自己 |
| SQLite 并发 | WAL + 连接池（每线程一个） | WAL + `database/sql` 连接池（天然支持） |
| 子进程管理 | `child_process.exec`（每命令一进程） | `os/exec`（同样开进程，但调度更轻） |

**Go 重写范围**（最小可行）：
- ✅ Runner（worker 池管理、autoscaler、集成器）
- ✅ Store（SQLite 操作、事件流）
- ✅ Harness（LLM API 调用、流式响应解析）
- ❌ 不重写 prompt 模板（保留 JS 版本，Go 通过 HTTP 调用）

**工作量**：1-2 周
- Go 的 `net/http` + `context.Context` 替代 Node 的 `fetch`
- Go 的 `mattn/go-sqlite3` 或 `modernc.org/sqlite` 替代 better-sqlite3
- Go 的 `os/exec` 替代 Node 的 `child_process`
- SSE 流式响应手动解析（`data: {...}\n\n` 格式）

**能撑到**：1000+ workers（甚至分布式部署）

---

## 决策树

```
当前 worker 数？
├── ≤ 200
│   └── 继续单进程 Node（现在的架构够用）
├── 200-500
│   └── 先优化单进程（流式 JSON、git 输出逐行读）
│       └── 还不够？
│           └── 上 worker_threads（1-2 天改造）
└── > 500
    └── worker_threads 撑不住了？
        └── Go 重写（1-2 周）
```

---

## 常见误区

### ❌ 误区 1：Go 一定比 Node 快

**真相**：
- 对 CPU 密集型任务（大规模 JSON 解析、正则匹配），Go 确实快 2-5 倍
- 但 swarm 的瓶颈不在 CPU，在 **I/O**（LLM API 调用、git 子进程、SQLite 读写）
- I/O 密集型任务，Node 和 Go 差距不大（都是异步非阻塞）

### ❌ 误区 2：worker_threads 能解决所有问题

**真相**：
- worker_threads 解决的是**单线程事件循环瓶颈**
- 但 SQLite 锁竞争、子进程爆炸、内存泄漏还是会撞墙
- 500 workers 时 worker_threads 够用，1000 workers 时还是要 Go

### ❌ 误区 3：现在换 Go 一劳永逸

**真相**：
- Go 重写是 1-2 周工作量，现在 100 workers 用不到
- 等真要上 1000 workers 了，你对 swarm 架构理解更深，重写会更准
- 现在换 Go = 开着小轿车挺好，非要换卡车拉货，但货还没到

---

## 实测数据（什么时候该升级）

### 升级到 worker_threads 的信号

- 事件循环延迟 > 100ms（用 `perf_hooks.monitorEventLoopDelay()` 监控）
- SQLite `SQLITE_BUSY` 错误率 > 5%
- 单个 worker 回合时间 > 2s（排除 LLM API 延迟后）

### 升级到 Go 的信号

- worker_threads IPC 通信成本 > 10% CPU
- 需要跨机器分布式部署（多台服务器跑同一个 swarm）
- 单台机器内存 > 32GB（worker_threads 独立堆，总内存爆炸）

---

## 总结

| 阶段 | worker 数 | 架构 | 工作量 | 现状 |
|---|---|---|---|---|
| 1️⃣ | ≤ 200 | 单进程 Node | 已完成 | ✅ 稳定运行 |
| 2️⃣ | 200-800 | worker_threads | 1-2 天 | 🔲 需要时再做 |
| 3️⃣ | 1000+ | Go 重写 | 1-2 周 | 🔲 远期目标 |

**当前建议**：继续用单进程 Node，等实测撞到墙了再升级。**不要现在换 Go**。
