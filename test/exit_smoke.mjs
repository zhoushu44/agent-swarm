// 自动退出冒烟：echo harness 驱动完整任务周期（claim→file→commit→submit→门禁→merge），
// 验证 board 清空 + 集成队列空 + worker 全退场时进程优雅退出。
// 这里直接用 Node API 复刻 cli.js 的 --exit-when-done 逻辑（echoScripts 测试注入）。
import path from 'node:path';
import fs from 'node:fs';
import { execSync } from 'node:child_process';
import { Store } from '../src/store.js';
import { Runner } from '../src/runner.js';
import { Repo } from '../src/gitutil.js';
import { createServer } from '../src/server.js';

const dir = path.join(process.env.TEMP, 'swarm-exit2');
execSync(`if exist "${dir}" rmdir /s /q "${dir}"`, { shell: 'cmd.exe' });
fs.mkdirSync(dir, { recursive: true });
execSync('git init -q', { cwd: dir });

new Repo(dir).init();
const store = new Store(path.join(dir, '.swarm', 'state.db'));
store.addItem({ id: 'task1', title: '创建 hello.txt', detail: '内容 hi', kind: 'task', priority: 5 });

const runner = new Runner({
  store, repoRoot: dir, serverPort: 17779,
  harness: 'echo', mission: 'test', minWorkers: 0, maxWorkers: 2,
});
runner.echoScripts = {
  '*': [
    'claim task1',
    'file hello.txt hi',
    'commit task1 done',
    'api POST /submit {"token":"{{token}}","peer":"{{peer}}","commitSha":""}',
  ].join('\n'),
};
await runner.spawnWorker('w1');
const { server } = await createServer(store, { port: 17779, getStatus: () => runner.status() });
runner.startIntegrator(); runner.startLeaseRenewer(); runner.startAutoscaler(); runner.startReaper();

let done = false;
const t0 = Date.now();
while (!done && Date.now() - t0 < 120000) {
  await new Promise((r) => setTimeout(r, 2000));
  try {
    const open = store.openCount();
    const queued = store.queuedSubmissions().length;
    const active = runner.status().active;
    console.log(`open=${open} queued=${queued} active=${active}`);
    if (open === 0 && queued === 0 && active === 0) done = true;
  } catch { break; }
}
const final = runner.status();
console.log('[swarm] DONE ' + JSON.stringify({
  done: final.stats.done || 0, total: final.stats.total || 0,
  integrations: (final.integrations || []).length, workers: final.workers.length,
}));
await runner.stop(); server.close(); store.close();
process.exit(done && final.stats.done === 1 ? 0 : 1);
