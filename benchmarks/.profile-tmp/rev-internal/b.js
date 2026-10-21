console.log(Object.keys(require.cache).filter(k => /internal|__/.test(k)));
require('tty'); require('util'); require('stream');
const ks = Object.keys(require.cache).filter(k => /internal|__/.test(k));
console.log(ks.length, ks.slice(0,20));
const u = require.cache['internal/util'] || require.cache['node:internal/util'];
console.log('internal/util via cache:', u && Object.keys(u.exports).slice(0,5));
// does a cached builtin module object expose its require?
const m = require.cache['node:fs'] || require.cache['fs'];
console.log('fs module keys', m && Object.keys(m));
// planting: put a fake entry for a user id that maps to internal? reverse: put internal into cache under user key
console.log(typeof globalThis.__otterRequireCache, typeof globalThis.primordials, typeof globalThis.internalBinding);
