// Evaluate the compat realm under this engine and report every audited
// `primordials` name it does not define.
//
// usage: node scripts/dev/primordials_check.mjs <names.json>
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;
const source = readFileSync(join(root, 'crates/otter-node/src/nodelib/compat/bootstrap_realm.js'), 'utf8');
const names = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const module = { exports: {} };
new Function('module', 'exports', 'require', source)(module, module.exports, () => ({}));
const { primordials } = module.exports;
const missing = names.filter((name) => !(name in primordials) || primordials[name] === undefined);
console.log(JSON.stringify({ defined: Object.keys(primordials).length, missing }));
