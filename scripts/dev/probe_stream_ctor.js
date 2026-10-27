// Construction of EventEmitter / Readable / Writable and emit, long enough to
// sample. usage: otter probe_stream_ctor.js <ee|emit|readable|writable> [n]
const { Readable, Writable } = require('node:stream');
const { EventEmitter } = require('node:events');
const which = process.argv[2] || 'readable';
const n = Number(process.argv[3] || 2_000_000);
let c = 0;
const t = performance.now();
if (which === 'ee') {
  for (let i = 0; i < n; i++) { const e = new EventEmitter(); e.on('x', (v) => { c += v; }); e.emit('x', 1); }
} else if (which === 'emit') {
  const e = new EventEmitter(); e.on('x', (v) => { c += v; }); e.on('x', (v) => { c -= v; });
  for (let i = 0; i < n; i++) e.emit('x', i);
} else if (which === 'readable') {
  for (let i = 0; i < n; i++) { const r = new Readable({ read() {} }); c += r.readableHighWaterMark & 1; }
} else {
  for (let i = 0; i < n; i++) { const w = new Writable({ write(ch, e, cb) { cb(); } }); c += w.writableHighWaterMark & 1; }
}
console.log(which, ((performance.now() - t) * 1e6 / n).toFixed(0), 'ns/op', c);
