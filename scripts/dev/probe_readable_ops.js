// Individual operations of node's Readable constructor, each in its own loop.
const { Readable } = require('node:stream');
const r = new Readable({ read() {} });
const plain = { a: 1 };
const kState = Symbol('kState');
const ops = {
  instanceofReadable: (n) => { let c = 0; for (let i = 0; i < n; i++) c += r instanceof Readable ? 1 : 0; return c; },
  constructNotNull: (n) => { let c = 0; for (let i = 0; i < n; i++) c += r._construct != null ? 1 : 0; return c; },
  eventsLoad: (n) => { let c = 0; for (let i = 0; i < n; i++) c += r._events === undefined ? 1 : 0; return c; },
  readLoad: (n) => { let c = 0; for (let i = 0; i < n; i++) c += typeof r._read === 'function' ? 1 : 0; return c; },
  protoEvents: (n) => { let c = 0; for (let i = 0; i < n; i++) c += Object.getPrototypeOf(r)._events === undefined ? 1 : 0; return c; },
  symbolOrStore: (n) => { const o = {}; o[kState] = 1; for (let i = 0; i < n; i++) o[kState] |= 2; return o[kState]; },
  nullishAssign: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; o.x ??= plain; c += o.x.a; } return c; },
  streamCall: (n) => { let c = 0; const S = Object.getPrototypeOf(Readable); for (let i = 0; i < n; i++) { const o = Object.create(Readable.prototype); S.call(o, {}); c++; } return c; },
};
for (const [name, f] of Object.entries(ops)) {
  f(2e5);
  const t = performance.now();
  f(2e6);
  console.log(name.padEnd(20), ((performance.now() - t) * 1e6 / 2e6).toFixed(1), 'ns');
}
