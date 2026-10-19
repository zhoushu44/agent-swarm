import { pathToFileURL } from 'node:url';

const modPath = process.argv[2];
const harness = process.argv[3] || 'direct';
const mod = await import(pathToFileURL(modPath).href);
const out = mod.buildWorkerPrompt({
  peerName: 'w1',
  serverPort: 17771,
  token: 'tok-abc',
  mission: 'port the swarm runtime to Go',
  conventions: 'go 1.22, no cgo',
  turn: 3,
  brief: 'BOARD SNAPSHOT: 1 pending',
  harness,
});
process.stdout.write(out);
