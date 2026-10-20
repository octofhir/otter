// Canonical dump of the JavaScript surface a launch starts with: every object
// reachable from globalThis (and from each node builtin's exports) to a fixed
// depth, by first-visit path, with each own property's descriptor shape,
// value type, function name/length, symbol registration and prototype
// identity. Getters are never called. Run it on a binary that restores its
// startup image and on one that builds the realm, then diff the outputs:
// any line that differs is state the image failed to carry.
//
// usage: otter scripts/dev/probe/image_surface_dump.js [depth] > out.txt
'use strict';
const maxDepth = Number(process.argv[2] ?? 4);
const ids = new Map();
const lines = [];
const queue = [];

function keyLabel(key) {
  if (typeof key === 'symbol') {
    const registered = Symbol.keyFor(key);
    return registered !== undefined ? `@@for(${registered})` : `@@(${key.description})`;
  }
  return JSON.stringify(key);
}

function visit(value, path, depth) {
  if ((typeof value !== 'object' && typeof value !== 'function') || value === null) return null;
  if (ids.has(value)) return ids.get(value);
  ids.set(value, path);
  if (depth < maxDepth) queue.push([value, path, depth]);
  return path;
}

function describe(value, path, depth) {
  if (typeof value === 'function') return `function name=${JSON.stringify(value.name)} length=${value.length} id=${visit(value, path, depth)}`;
  if (typeof value === 'object' && value !== null) return `object id=${visit(value, path, depth)}`;
  if (typeof value === 'symbol') return `symbol ${keyLabel(value)}`;
  if (typeof value === 'boolean' || value === null || value === undefined) return String(value);
  return typeof value;
}

function walk() {
  while (queue.length > 0) {
    const [object, path, depth] = queue.shift();
    let keys;
    try {
      keys = Reflect.ownKeys(object);
    } catch (error) {
      lines.push(`${path} !ownKeys ${error && error.name}`);
      continue;
    }
    let proto;
    try {
      proto = Object.getPrototypeOf(object);
    } catch (error) {
      proto = undefined;
    }
    lines.push(`${path} proto=${proto === null ? 'null' : describe(proto, `${path}.[[Prototype]]`, depth + 1)} extensible=${Object.isExtensible(object)}`);
    for (const key of keys) {
      let descriptor;
      try {
        descriptor = Reflect.getOwnPropertyDescriptor(object, key);
      } catch (error) {
        lines.push(`${path}[${keyLabel(key)}] !descriptor ${error && error.name}`);
        continue;
      }
      if (descriptor === undefined) continue;
      const child = `${path}[${keyLabel(key)}]`;
      const flags = `${descriptor.enumerable ? 'e' : '-'}${descriptor.configurable ? 'c' : '-'}`;
      if ('value' in descriptor) {
        lines.push(`${child} data ${descriptor.writable ? 'w' : '-'}${flags} ${describe(descriptor.value, child, depth + 1)}`);
      } else {
        lines.push(`${child} accessor ${flags} get=${descriptor.get ? describe(descriptor.get, `${child}.get`, depth + 1) : '-'} set=${descriptor.set ? describe(descriptor.set, `${child}.set`, depth + 1) : '-'}`);
      }
    }
  }
}

visit(globalThis, 'globalThis', 0);
walk();
const { builtinModules } = require('node:module');
for (const name of [...builtinModules].sort()) {
  let exports;
  try {
    exports = require(name);
  } catch (error) {
    lines.push(`require(${name}) !throws ${error && error.name}`);
    continue;
  }
  lines.push(`require(${name}) ${describe(exports, `require(${name})`, maxDepth - 2)}`);
  walk();
}
process.stdout.write(`${lines.join('\n')}\n`);
