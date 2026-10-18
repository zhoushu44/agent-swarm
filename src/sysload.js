// sysload —— 系统资源护栏的探测与裁决（纯函数 + 一个差分采样器）。
// 背景事故：某次 run 以 --max-workers 12 --max-inflight 64 跑在一台普通 PC 上，
// 外加外部看门狗保活，直接把机器 CPU/内存耗干卡死。整个项目此前没有任何
// os.cpus/loadavg/freemem 感知——机器多大都敢开 N 个 worker。本模块补上这一层。
//
// 跨平台要点：Windows 上 os.loadavg() 恒为 [0,0,0]，不能用作负载信号；
// 必须用 os.cpus() 的 times 做两次采样差分算出 CPU 占用率。
import os from 'node:os';

let lastSample = null;

// 采集一次系统快照。cpuBusy 需要两次采样差分，首次调用返回 null（调用方按"未知"处理，
// 不要因为首样本缺失而误判高压）。本函数有内部状态（lastSample），测试请用
// makeSampler() 拿独立实例，或用纯函数 guardCeiling 直接喂合成样本。
export function sampleSystem() {
  return _sample((s) => { lastSample = s; }, () => lastSample);
}

// 测试用：返回一个带独立状态的采样器，互不污染。
export function makeSampler() {
  let last = null;
  return () => _sample((s) => { last = s; }, () => last);
}

function _sample(setLast, getLast) {
  const cpus = os.cpus();
  const n = cpus.length || 1;
  let idle = 0, total = 0;
  for (const c of cpus) {
    idle += c.times.idle;
    total += c.times.user + c.times.nice + c.times.sys + c.times.irq + c.times.idle;
  }
  const prev = getLast();
  let cpuBusy = null;
  if (prev && total > prev.total) {
    cpuBusy = Math.max(0, Math.min(1, 1 - (idle - prev.idle) / (total - prev.total)));
  }
  setLast({ idle, total });
  const totalMem = os.totalmem();
  const freeMemRatio = totalMem > 0 ? os.freemem() / totalMem : 1;
  return { cpus: n, cpuBusy, freeMemRatio };
}

// 机器静态上限：CPU 核数的一半（向上取整，下限 2）。
// 依据：worker 的大头是等 API（网络等待），但每回合的 git 操作和门禁 testCmd
// 是实打实的 CPU/磁盘密集；超过核数一半的并发在门禁集中触发时会把机器打满。
export function machineCeiling(cpus) {
  return Math.max(2, Math.ceil((cpus || 1) / 2));
}

// 纯函数裁决：给定系统快照与当前活跃 worker 数，返回 { ceiling, pressure }。
// ceiling = null 表示不设上限（尊重用户显式指定的 --max-workers）。
//
// 语义刻意做成"只拦增长，不替用户做决定"：
//   ok       → ceiling=null，用户 maxWorkers 说了算（机器健康就别多管）
//   high     → ceiling=active，只许停不许扩（系统已 >80% CPU 或可用内存 <15%）
//   critical → ceiling=active-1，主动缩一个（>92% CPU 或可用内存 <8%，再不缩就卡死）
// 阈值依据：cpuBusy>0.8 时进程已开始明显排队；可用内存 <15% 时 Windows 开始频繁换页。
export function guardCeiling(sample, { active = 0 } = {}) {
  let pressure = 'ok';
  if (sample.freeMemRatio < 0.08) pressure = 'critical';
  else if (sample.freeMemRatio < 0.15) pressure = 'high';
  if (sample.cpuBusy != null) {
    if (sample.cpuBusy > 0.92) pressure = 'critical';
    else if (sample.cpuBusy > 0.8 && pressure === 'ok') pressure = 'high';
  }
  let ceiling = null;
  if (pressure === 'high') ceiling = Math.max(1, active);
  else if (pressure === 'critical') ceiling = Math.max(1, active - 1);
  return { ceiling, pressure };
}
