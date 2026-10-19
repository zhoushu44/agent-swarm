import fs from 'fs';
const names = ['full', 'empty-cli', 'minimal-direct', 'brief-only'];
for (const n of names) {
  const a = fs.readFileSync(`ref_${n}.txt`, 'utf8');
  const b = fs.readFileSync(`internal/prompts/testdata/${n}.golden`, 'utf8');
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  console.log(n, 'reflen', a.length, 'golen', b.length, 'firstdiff', i);
  if (i < Math.max(a.length, b.length)) {
    const lo = Math.max(0, i - 70);
    console.log(' REF:', JSON.stringify(a.slice(lo, lo + 160)));
    console.log(' GOT:', JSON.stringify(b.slice(lo, lo + 160)));
  }
}
