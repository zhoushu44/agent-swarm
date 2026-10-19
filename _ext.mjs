import fs from 'fs';
import * as P from '../../../../swarm/src/prompts.js';

const full = P.buildWorkerPrompt({ peerName: 'P', serverPort: 1, mission: 'M', conventions: 'C', harness: 'direct' });
const cli = P.buildWorkerPrompt({ peerName: 'P', serverPort: 1, mission: 'M', conventions: 'C', harness: 'cli' });

function segs(s) {
  const iTools = s.indexOf('TOOLS:');
  const iLoop = s.indexOf('Follow this loop, in order:');
  const iRules = s.indexOf('HARD RULES:');
  const iIdent = s.indexOf('YOUR IDENTITY');
  return {
    head: s.slice(0, iTools),
    tools: s.slice(iTools, iLoop - 2),
    loop: s.slice(iLoop - 2, iRules),
    rules: s.slice(iRules, iIdent),
    tail: s.slice(iIdent),
  };
}

const d = segs(full);
const c = segs(cli);
const out = { head: d.head, toolsDirect: d.tools, toolsCLI: c.tools, loop: d.loop, rules: d.rules, tail: d.tail };
fs.writeFileSync('_segs.json', JSON.stringify(out, null, 2), 'utf8');
for (const [k, v] of Object.entries(out)) console.log(k, v.length);
