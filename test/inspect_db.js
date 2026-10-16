import { DatabaseSync } from 'node:sqlite';
const db = new DatabaseSync(process.argv[2], { readOnly: true });
const rows = db.prepare('SELECT peer, item_id, test_cmd, test_passed, merged FROM integrations LIMIT 5').all();
console.log('integrations:', JSON.stringify(rows, null, 1));
const evs = db.prepare("SELECT payload FROM events WHERE type='integration.failed' LIMIT 3").all();
for (const e of evs) console.log('FAIL:', String(e.payload).slice(0, 1500));
