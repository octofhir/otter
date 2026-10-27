// Pieces of node's Stream/EventEmitter constructor chain.
const EE = require('node:events');
const { Readable } = require('node:stream');
const Stream = Object.getPrototypeOf(Readable);
const kCapture = Object.getOwnPropertySymbols(EE.prototype)[0];
function Plain() {}
function PlainInit() { this.x = 1; }
const ops = {
  newEE: (n) => { let c = 0; for (let i = 0; i < n; i++) { const e = new EE(); c++; } return c; },
  initCall: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; EE.init.call(o, undefined); c++; } return c; },
  eeCall: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; EE.call(o, undefined); c++; } return c; },
  streamCall: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; Stream.call(o, undefined); c++; } return c; },
  plainCall: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; PlainInit.call(o); c += o.x; } return c; },
  protoOfEvents: (n) => { let c = 0; const o = new EE(); for (let i = 0; i < n; i++) c += Object.getPrototypeOf(o)._events === undefined ? 1 : 0; return c; },
  captureLoad: (n) => { let c = 0; for (let i = 0; i < n; i++) c += EE.prototype[kCapture] ? 1 : 0; return c; },
  maxOrAssign: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = {}; o._maxListeners ||= undefined; c++; } return c; },
  nullProtoLit: (n) => { let c = 0; for (let i = 0; i < n; i++) { const o = { __proto__: null }; c++; } return c; },
};
for (const [name, f] of Object.entries(ops)) {
  f(2e5);
  const t = performance.now();
  f(2e6);
  console.log(name.padEnd(16), ((performance.now() - t) * 1e6 / 2e6).toFixed(1), 'ns');
}
