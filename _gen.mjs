import fs from 'fs';
import * as P from '../../../../swarm/src/prompts.js';

const cases = {
  full: { peerName: 'w6', serverPort: 17771, token: 'tok-abc', mission: 'Build the thing', conventions: 'Go 1.22, no scratch files', turn: 3, brief: 'BOARD SNAPSHOT: 2 pending / 0 done.', harness: 'direct' },
  'empty-cli': { peerName: 'w1', serverPort: 8080, harness: 'cli' },
  'minimal-direct': { peerName: 'w9', serverPort: 9090, mission: 'm', harness: 'direct' },
  'brief-only': { peerName: 'w2', serverPort: 1234, token: 'zzz', mission: 'Mission', conventions: 'Conv', turn: 1, brief: 'BRIEF', harness: 'direct' },
};

for (const [name, opts] of Object.entries(cases)) {
  fs.writeFileSync(`ref_${name}.txt`, P.buildWorkerPrompt(opts), 'utf8');
}
console.log('ok');
