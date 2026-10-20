// Exercises the APIs whose state a launch might take from the startup image:
// every line is one call and what it returned or threw. Run it on a launch
// that restores the image and on one that builds the realm (`--expose-gc`
// changes the surface fingerprint, so that launch builds), then diff: a line
// that differs names state the image failed to carry.
//
// usage: otter scripts/dev/probe/image_api_battery.js > out.txt
'use strict';
const lines = [];
async function probe(name, fn) {
  try {
    const value = await fn();
    lines.push(`${name}: ${typeof value === 'string' ? value : JSON.stringify(value)}`);
  } catch (error) {
    lines.push(`${name}: threw ${error && error.name}: ${error && error.message}`);
  }
}

const wasmAdd = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 1, 7, 1, 96, 2, 127, 127, 1, 127, 3, 2, 1, 0, 7, 7, 1, 3, 97, 100, 100, 0, 0, 10, 9, 1, 7, 0, 32, 0, 32, 1, 106, 11]);

(async () => {
  await probe('wasm.validate', () => WebAssembly.validate(wasmAdd));
  await probe('wasm.instantiate', async () => (await WebAssembly.instantiate(wasmAdd)).instance.exports.add(2, 3));
  await probe('wasm.Memory', () => new WebAssembly.Memory({ initial: 1 }).buffer.byteLength);
  await probe('wasm.Global', () => new WebAssembly.Global({ value: 'i32', mutable: true }, 7).value);
  await probe('wasm.Table', () => new WebAssembly.Table({ initial: 2, element: 'anyfunc' }).length);
  await probe('wasm.JSTag', () => new WebAssembly.Exception(WebAssembly.JSTag, [1]) instanceof WebAssembly.Exception);
  await probe('crypto.getRandomValues', () => crypto.getRandomValues(new Uint8Array(8)).length);
  await probe('crypto.randomUUID', () => crypto.randomUUID().length);
  await probe('crypto.subtle.digest', async () => new Uint8Array(await crypto.subtle.digest('SHA-256', new Uint8Array([1, 2, 3])))[0]);
  await probe('node crypto hash', () => require('node:crypto').createHash('sha1').update('x').digest('hex'));
  await probe('URL', () => new URL('https://a.b/c?d=1#e').searchParams.get('d'));
  await probe('URL searchParams identity', () => { const u = new URL('http://x/?a=1'); return u.searchParams === u.searchParams; });
  await probe('URLSearchParams', () => new URLSearchParams('a=1&b=2').toString());
  await probe('URLPattern', () => new URLPattern({ pathname: '/a/:id' }).exec('https://x/a/7').pathname.groups.id);
  await probe('Blob', async () => await new Blob(['ab', 'c']).text());
  await probe('File', () => new File(['x'], 'f.txt').name);
  await probe('TextEncoder', () => Array.from(new TextEncoder().encode('é')));
  await probe('TextDecoder', () => new TextDecoder().decode(new Uint8Array([195, 169])));
  await probe('structuredClone', () => structuredClone({ a: [1, { b: 2 }], d: new Date(0) }).a[1].b);
  await probe('MessageChannel', () => new Promise((resolve) => { const c = new MessageChannel(); c.port2.onmessage = (e) => { resolve(e.data); c.port1.close(); c.port2.close(); }; c.port1.postMessage('hi'); }));
  await probe('EventTarget', () => { const t = new EventTarget(); let n = 0; t.addEventListener('x', () => n++); t.dispatchEvent(new Event('x')); return n; });
  await probe('AbortController', () => { const c = new AbortController(); c.abort('r'); return [c.signal.aborted, c.signal.reason]; });
  await probe('AbortSignal.timeout', () => typeof AbortSignal.timeout(10));
  await probe('performance.now small', () => performance.now() < 5000);
  await probe('performance.timeOrigin recent', () => Math.abs(performance.timeOrigin - Date.now()) < 5000);
  await probe('perf_hooks', () => Math.abs(require('node:perf_hooks').performance.timeOrigin - Date.now()) < 5000);
  await probe('Headers', () => new Headers({ a: '1' }).get('a'));
  await probe('Response', async () => await new Response('body').text());
  await probe('Request', () => new Request('https://x/', { method: 'POST' }).method);
  await probe('ReadableStream', async () => { const r = new ReadableStream({ start(c) { c.enqueue(1); c.close(); } }).getReader(); return (await r.read()).value; });
  await probe('CompressionStream', async () => { const s = new Blob(['aaaa']).stream().pipeThrough(new CompressionStream('gzip')); const b = await new Response(s).arrayBuffer(); return b.byteLength > 0; });
  await probe('TextEncoderStream', () => typeof new TextEncoderStream().writable);
  await probe('setTimeout', () => new Promise((resolve) => setTimeout(() => resolve('t'), 1)));
  await probe('setImmediate', () => new Promise((resolve) => setImmediate(() => resolve('i'))));
  await probe('queueMicrotask', () => new Promise((resolve) => queueMicrotask(() => resolve('m'))));
  await probe('process.hrtime small', () => process.hrtime()[0] < 5);
  await probe('process.uptime small', () => process.uptime() < 5);
  await probe('process.cwd', () => process.cwd() === require('node:path').resolve('.'));
  await probe('process.pid', () => process.pid > 0 && process.pid !== 1);
  await probe('process.argv1', () => require('node:path').basename(process.argv[1]));
  await probe('process.env HOME', () => typeof process.env.HOME);
  await probe('process.memoryUsage', () => process.memoryUsage().heapUsed > 100000);
  await probe('Buffer', () => Buffer.from('hi').toString('base64'));
  await probe('fs', () => require('node:fs').existsSync(__filename));
  await probe('os', () => typeof require('node:os').cpus().length);
  await probe('util.inspect', () => require('node:util').inspect({ a: new Map([[1, 2]]) }));
  await probe('events', () => { const E = require('node:events'); const e = new E(); let n = 0; e.on('x', () => n++); e.emit('x'); return n; });
  await probe('getEventListeners', () => { const t = new EventTarget(); const f = () => {}; t.addEventListener('x', f); return require('node:events').getEventListeners(t, 'x').length; });
  await probe('zlib', () => require('node:zlib').gunzipSync(require('node:zlib').gzipSync('abc')).toString());
  await probe('Intl', () => new Intl.NumberFormat('en-US').format(1234.5));
  await probe('Temporal', () => typeof Temporal.Now.instant().epochMilliseconds);
  await probe('Date now', () => Math.abs(Date.now() - new Date().getTime()) < 1000);
  await probe('Math.random distinct', () => Math.random() !== Math.random());
  await probe('WeakRef', () => { const o = {}; return new WeakRef(o).deref() === o; });
  await probe('FinalizationRegistry', () => typeof new FinalizationRegistry(() => {}).register);
  await probe('Symbol.for registry', () => Symbol.keyFor(Object.getOwnPropertySymbols(new EventTarget())[0]));
  await probe('console.log returns', () => typeof console.log);
  await probe('worker_threads', () => typeof require('node:worker_threads').Worker);
  await probe('child_process', () => typeof require('node:child_process').spawnSync);
  await probe('string_decoder', () => new (require('node:string_decoder').StringDecoder)('utf8').write(Buffer.from([0xe2, 0x82, 0xac])));
  await probe('assert', () => { require('node:assert').deepStrictEqual({ a: 1 }, { a: 1 }); return 'ok'; });
  await probe('module createRequire', () => typeof require('node:module').createRequire(__filename)('node:path').join);
  await probe('Otter global', () => typeof globalThis.Otter);
  await probe('navigator', () => typeof navigator.userAgent);
  await probe('atob/btoa', () => atob(btoa('xyz')));
  await probe('BroadcastChannel', () => typeof BroadcastChannel);
  await probe('WebSocket', () => typeof WebSocket);
  process.stdout.write(`${lines.join('\n')}\n`);
})();
