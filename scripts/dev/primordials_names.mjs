// Every `primordials` name the node lib sources read: destructured from
// `primordials` or read as `primordials.<name>`. Parsed with acorn, never
// pattern-matched.
//
// usage: node scripts/dev/primordials_names.mjs > names.json
import { createRequire } from 'node:module';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const root = new URL('../..', import.meta.url).pathname;
const require = createRequire(join(root, 'docs/site/node_modules/.pnpm/acorn@8.16.0/node_modules/acorn/package.json'));
const acorn = require('./dist/acorn.js');
const lib = join(root, 'crates/otter-node/src/nodelib');

const files = [];
(function walk(dir) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path);
    else if (name.endsWith('.js')) files.push(path);
  }
})(lib);

const names = new Map();
function note(name, file) {
  if (!names.has(name)) names.set(name, new Set());
  names.get(name).add(relative(lib, file));
}
function visit(node, file) {
  if (!node || typeof node.type !== 'string') return;
  if (node.type === 'VariableDeclarator' && node.init?.type === 'Identifier' &&
      node.init.name === 'primordials' && node.id.type === 'ObjectPattern') {
    for (const prop of node.id.properties) {
      if (prop.type === 'Property' && !prop.computed && prop.key.type === 'Identifier') note(prop.key.name, file);
    }
  }
  if (node.type === 'MemberExpression' && !node.computed && node.object.type === 'Identifier' &&
      node.object.name === 'primordials' && node.property.type === 'Identifier') {
    note(node.property.name, file);
  }
  for (const key of Object.keys(node)) {
    const child = node[key];
    if (Array.isArray(child)) child.forEach((c) => visit(c, file));
    else if (child && typeof child.type === 'string') visit(child, file);
  }
}
for (const file of files) {
  const text = readFileSync(file, 'utf8');
  let ast;
  try {
    ast = acorn.parse(text, { ecmaVersion: 'latest', sourceType: 'script', allowReturnOutsideFunction: true, allowHashBang: true });
  } catch (error) {
    console.error(`skip ${relative(lib, file)}: ${error.message}`);
    continue;
  }
  visit(ast, file);
}
console.log(JSON.stringify([...names.keys()].sort(), null, 0));
