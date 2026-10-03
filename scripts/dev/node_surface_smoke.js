// A pass over the node lib surfaces that load the vendored realm: util,
// events, streams, timers, buffer and string decoding.
const util = require('util');
const { EventEmitter, once } = require('events');
const { Readable, Transform, pipeline } = require('stream');
const { StringDecoder } = require('string_decoder');
const out = [];
out.push(util.inspect({ a: [1, 2, { b: new Map([[1, 'x']]) }], s: new Set([1]) }, { depth: 4 }));
out.push(util.format('%s:%d:%j', 'k', 42, { z: 1 }));
const emitter = new EventEmitter();
emitter.on('tick', (n) => out.push(`tick ${n}`));
emitter.emit('tick', 1);
const decoder = new StringDecoder('utf8');
out.push(decoder.write(Buffer.from([0xe2, 0x82])) + decoder.end(Buffer.from([0xac])));
const upper = new Transform({ transform(chunk, _enc, done) { done(null, chunk.toString().toUpperCase()); } });
const chunks = [];
pipeline(Readable.from(['ab', 'cd']), upper, async function* (source) {
  for await (const chunk of source) chunks.push(chunk.toString());
}, (error) => {
  out.push(error ? `error ${error.message}` : `piped ${chunks.join('')}`);
  setTimeout(async () => {
    setImmediate(() => emitter.emit('done', 7));
    const [value] = await once(emitter, 'done');
    out.push(`once ${value}`);
    console.log(out.join('\n'));
  }, 1);
});
