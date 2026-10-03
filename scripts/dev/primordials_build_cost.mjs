// Build the compat realm's primordials repeatedly so a sampling profile
// sees where eager construction spends its time.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;
const source = readFileSync(join(root, 'crates/otter-node/src/nodelib/compat/bootstrap_realm.js'), 'utf8');
const build = new Function('module', 'exports', 'require', source);
const rounds = Number(process.argv[2] ?? 20);
let count = 0;
const t0 = Date.now();
for (let i = 0; i < rounds; i++) {
  const module = { exports: {} };
  build(module, module.exports, () => ({}));
  count = Reflect.ownKeys(module.exports.primordials).length;
}
console.log(count, 'entries', ((Date.now() - t0) / rounds).toFixed(2), 'ms per build');
