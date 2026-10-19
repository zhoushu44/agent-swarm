// 测试入口：跑离线协议闭环测试。
// 真实 harness 测试（claude/codex）需要 API 额度，另行手动跑。
import { run as offlineRun } from './offline_swarm_test.js';
import { run as protocolRun } from './protocol_test.js';
import { run as apiRun } from './api_test.js';
import { run as resourceGuardRun } from './resource_guard_test.js';
import { run as aimdRun } from './aimd_test.js';

const suites = [
  ['resource_guard', resourceGuardRun],
  ['aimd', aimdRun],
  ['offline_swarm', offlineRun],
  ['protocol', protocolRun],
  ['api', apiRun],
];

let failed = 0;
for (const [name, fn] of suites) {
  console.log(`\n=== ${name} ===`);
  try {
    await fn();
  } catch (e) {
    failed += 1;
    console.error(`${name} FAILED:`, e.message);
  }
}
console.log(failed ? `\n${failed} suite(s) failed` : '\nall suites passed');
process.exit(failed ? 1 : 0);
