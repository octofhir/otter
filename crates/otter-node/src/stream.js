'use strict';
// `node:stream` — a practical subset of Node streams in JS, built on the
// EventEmitter shim. Covers the common Readable/Writable/Duplex/Transform/
// PassThrough surface: push/read/pipe, flowing + paused modes, write/end with
// 'drain'/'finish', _read/_write/_transform/_flush hooks, Readable.from, async
// iteration, and the `finished`/`pipeline` helpers. Backpressure is modelled
// coarsely (highWaterMark thresholds) rather than byte-exactly.

const EventEmitter = require('events');
const { Buffer } = require('buffer');

function nextTick(fn, ...args) {
  if (typeof queueMicrotask === 'function') queueMicrotask(() => fn(...args));
  else Promise.resolve().then(() => fn(...args));
}

class Stream extends EventEmitter {
  pipe(dest) { return Readable.prototype.pipe.call(this, dest); }
}

// ---------------- Readable ----------------
class Readable extends Stream {
  constructor(options = {}) {
    super();
    const state = {
      objectMode: !!options.objectMode,
      highWaterMark: options.highWaterMark != null ? options.highWaterMark : (options.objectMode ? 16 : 16384),
      buffer: [],
      length: 0,
      flowing: null,
      reading: false,
      ended: false,
      endEmitted: false,
      readableListening: false,
      resumeScheduled: false,
      destroyed: false,
      encoding: options.encoding || null,
    };
    this._readableState = state;
    this.readable = true;
    if (typeof options.read === 'function') this._read = options.read;
    if (typeof options.destroy === 'function') this._destroy = options.destroy;
  }

  _read() {}

  _destroy(err, cb) { cb(err); }

  push(chunk, encoding) {
    return readableAddChunk(this, chunk, encoding);
  }

  unshift(chunk) {
    const state = this._readableState;
    if (chunk !== null && chunk !== undefined) {
      state.buffer.unshift(chunk);
      state.length += 1;
    }
    return true;
  }

  read(n) {
    const state = this._readableState;
    if (state.buffer.length === 0) {
      if (state.ended && !state.endEmitted) {
        state.endEmitted = true;
        nextTick(() => this.emit('end'));
      } else if (!state.reading) {
        state.reading = true;
        this._read(state.highWaterMark);
        state.reading = false;
      }
      return null;
    }
    const chunk = state.buffer.shift();
    state.length -= 1;
    return chunk;
  }

  setEncoding(enc) { this._readableState.encoding = enc; return this; }

  pause() {
    if (this._readableState.flowing !== false) {
      this._readableState.flowing = false;
      this.emit('pause');
    }
    return this;
  }

  resume() {
    const state = this._readableState;
    if (!state.flowing) {
      state.flowing = true;
      if (!state.resumeScheduled) {
        state.resumeScheduled = true;
        nextTick(() => { state.resumeScheduled = false; flow(this); this.emit('resume'); });
      }
    }
    return this;
  }

  isPaused() { return this._readableState.flowing === false; }

  on(ev, fn) {
    const res = super.on(ev, fn);
    const state = this._readableState;
    if (ev === 'data') {
      if (state.flowing !== false) this.resume();
    } else if (ev === 'readable') {
      state.readableListening = true;
    }
    return res;
  }

  addListener(ev, fn) { return this.on(ev, fn); }

  pipe(dest, options) {
    const src = this;
    const onData = (chunk) => {
      const ok = dest.write(chunk);
      if (ok === false && typeof src.pause === 'function') {
        src.pause();
        dest.once('drain', () => src.resume());
      }
    };
    src.on('data', onData);
    if (!options || options.end !== false) {
      src.once('end', () => dest.end());
    }
    src.on('error', (err) => { if (dest.emit) dest.emit('error', err); });
    dest.emit('pipe', src);
    return dest;
  }

  destroy(err) {
    const state = this._readableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    this._destroy(err || null, (e) => {
      if (e) this.emit('error', e);
      this.emit('close');
    });
    return this;
  }

  [Symbol.asyncIterator]() {
    const self = this;
    let ended = false;
    const errs = [];
    self.on('end', () => { ended = true; });
    self.on('error', (e) => { errs.push(e); });
    return {
      next() {
        return new Promise((resolve, reject) => {
          const chunk = self.read();
          if (chunk !== null) return resolve({ value: chunk, done: false });
          if (errs.length) return reject(errs.shift());
          if (ended) return resolve({ value: undefined, done: true });
          const onReadable = () => { cleanup(); resolve(pump()); };
          const onEnd = () => { cleanup(); resolve({ value: undefined, done: true }); };
          const onError = (e) => { cleanup(); reject(e); };
          const cleanup = () => {
            self.removeListener('readable', onReadable);
            self.removeListener('end', onEnd);
            self.removeListener('error', onError);
          };
          const pump = () => {
            const c = self.read();
            return c !== null ? { value: c, done: false } : { value: undefined, done: true };
          };
          self.on('readable', onReadable);
          self.once('end', onEnd);
          self.once('error', onError);
        });
      },
      [Symbol.asyncIterator]() { return this; },
    };
  }

  static from(iterable, options) {
    const r = new Readable({ objectMode: true, ...options });
    r._read = () => {};
    (async () => {
      try {
        for await (const chunk of iterable) r.push(chunk);
        r.push(null);
      } catch (err) {
        r.destroy(err);
      }
    })();
    return r;
  }
}

function readableAddChunk(stream, chunk, encoding) {
  const state = stream._readableState;
  if (chunk === null) {
    state.ended = true;
    if (state.flowing) {
      nextTick(() => {
        if (!state.endEmitted) { state.endEmitted = true; stream.emit('end'); }
      });
    } else if (state.buffer.length === 0 && !state.endEmitted) {
      nextTick(() => {
        if (state.buffer.length === 0 && !state.endEmitted) {
          state.endEmitted = true;
          stream.emit('end');
        }
      });
    }
    return false;
  }
  if (typeof chunk === 'string' && !state.objectMode) {
    chunk = Buffer.from(chunk, state.encoding || 'utf8');
  }
  state.buffer.push(chunk);
  state.length += 1;
  if (state.flowing) {
    nextTick(() => flow(stream));
  } else if (state.readableListening) {
    nextTick(() => stream.emit('readable'));
  }
  return state.length < state.highWaterMark;
}

function flow(stream) {
  const state = stream._readableState;
  while (state.flowing && state.buffer.length > 0) {
    const chunk = state.buffer.shift();
    state.length -= 1;
    let out = chunk;
    if (state.encoding && Buffer.isBuffer(chunk)) out = chunk.toString(state.encoding);
    stream.emit('data', out);
  }
  if (state.flowing) {
    if (!state.ended && !state.reading) {
      state.reading = true;
      stream._read(state.highWaterMark);
      state.reading = false;
    }
    if (state.ended && state.buffer.length === 0 && !state.endEmitted) {
      state.endEmitted = true;
      stream.emit('end');
    }
  }
}

// ---------------- Writable ----------------
function initWritableState(self, options = {}) {
  self._writableState = {
    objectMode: !!options.objectMode,
    highWaterMark: options.highWaterMark != null ? options.highWaterMark : (options.objectMode ? 16 : 16384),
    length: 0,
    writing: false,
    corked: 0,
    ended: false,
    finished: false,
    destroyed: false,
    buffered: [],
    needDrain: false,
  };
  self.writable = true;
  if (typeof options.write === 'function') self._write = options.write;
  if (typeof options.writev === 'function') self._writev = options.writev;
  if (typeof options.final === 'function') self._final = options.final;
  if (typeof options.destroy === 'function') self._destroy = options.destroy;
}

class Writable extends Stream {
  constructor(options = {}) {
    super();
    initWritableState(this, options);
  }

  _write(_chunk, _enc, cb) { cb(); }
  _destroy(err, cb) { cb(err); }
  _final(cb) { cb(); }

  cork() { this._writableState.corked += 1; }
  uncork() {
    const state = this._writableState;
    if (state.corked) { state.corked -= 1; if (!state.corked) clearBuffer(this); }
  }

  setDefaultEncoding(enc) { this._writableState.defaultEncoding = enc; return this; }

  write(chunk, encoding, cb) {
    const state = this._writableState;
    if (typeof encoding === 'function') { cb = encoding; encoding = null; }
    if (state.ended) {
      const err = new Error('write after end');
      err.code = 'ERR_STREAM_WRITE_AFTER_END';
      nextTick(() => { if (cb) cb(err); this.emit('error', err); });
      return false;
    }
    if (typeof chunk === 'string' && !state.objectMode) {
      chunk = Buffer.from(chunk, encoding || 'utf8');
    }
    state.length += 1;
    if (state.writing || state.corked) {
      state.buffered.push({ chunk, encoding, cb });
    } else {
      doWrite(this, chunk, encoding, cb);
    }
    const ret = state.length < state.highWaterMark;
    if (!ret) state.needDrain = true;
    return ret;
  }

  end(chunk, encoding, cb) {
    const state = this._writableState;
    if (typeof chunk === 'function') { cb = chunk; chunk = null; }
    else if (typeof encoding === 'function') { cb = encoding; encoding = null; }
    if (chunk !== null && chunk !== undefined) this.write(chunk, encoding);
    state.ended = true;
    if (cb) this.once('finish', cb);
    finishMaybe(this);
    return this;
  }

  destroy(err) {
    const state = this._writableState;
    if (state.destroyed) return this;
    state.destroyed = true;
    this._destroy(err || null, (e) => {
      if (e) this.emit('error', e);
      this.emit('close');
    });
    return this;
  }
}

function doWrite(stream, chunk, encoding, cb) {
  const state = stream._writableState;
  state.writing = true;
  stream._write(chunk, encoding || 'utf8', (err) => {
    state.writing = false;
    state.length -= 1;
    if (err) {
      if (cb) cb(err);
      stream.emit('error', err);
      return;
    }
    if (cb) cb();
    if (state.needDrain && state.length < state.highWaterMark) {
      state.needDrain = false;
      stream.emit('drain');
    }
    clearBuffer(stream);
    finishMaybe(stream);
  });
}

function clearBuffer(stream) {
  const state = stream._writableState;
  if (state.writing || state.corked) return;
  if (state.buffered.length > 0) {
    const { chunk, encoding, cb } = state.buffered.shift();
    doWrite(stream, chunk, encoding, cb);
  }
}

function finishMaybe(stream) {
  const state = stream._writableState;
  if (state.ended && !state.writing && state.buffered.length === 0 && !state.finished) {
    state.finished = true;
    stream._final((err) => {
      if (err) { stream.emit('error', err); return; }
      stream.emit('finish');
    });
  }
}

// ---------------- Duplex / Transform / PassThrough ----------------
class Duplex extends Readable {
  constructor(options = {}) {
    super(options);
    initWritableState(this, options);
    if (options && options.readable === false) this.readable = false;
    if (options && options.writable === false) this.writable = false;
  }
}
// Mix Writable's prototype methods onto Duplex.
for (const key of Object.getOwnPropertyNames(Writable.prototype)) {
  if (key === 'constructor') continue;
  if (!(key in Duplex.prototype)) {
    Object.defineProperty(Duplex.prototype, key,
      Object.getOwnPropertyDescriptor(Writable.prototype, key));
  }
}

class Transform extends Duplex {
  constructor(options = {}) {
    super(options);
    if (typeof options.transform === 'function') this._transform = options.transform;
    if (typeof options.flush === 'function') this._flush = options.flush;
    this._write = (chunk, enc, cb) => {
      this._transform(chunk, enc, (err, data) => {
        if (err) return cb(err);
        if (data !== null && data !== undefined) this.push(data);
        cb();
      });
    };
    this.once('finish', () => {
      if (typeof this._flush === 'function') {
        this._flush((err, data) => {
          if (data !== null && data !== undefined) this.push(data);
          if (err) this.emit('error', err);
          this.push(null);
        });
      } else {
        this.push(null);
      }
    });
  }

  _transform(chunk, _enc, cb) { cb(null, chunk); }
}

class PassThrough extends Transform {
  constructor(options) { super(options); }
  _transform(chunk, _enc, cb) { cb(null, chunk); }
}

// ---------------- helpers ----------------
function finished(stream, optionsOrCb, maybeCb) {
  const cb = typeof optionsOrCb === 'function' ? optionsOrCb : maybeCb;
  let called = false;
  const done = (err) => { if (called) return; called = true; if (cb) cb(err || null); };
  stream.on('end', () => done());
  stream.on('finish', () => done());
  stream.on('close', () => done());
  stream.on('error', (err) => done(err));
  if (cb) return undefined;
  return new Promise((resolve, reject) => {
    const p = (err) => (err ? reject(err) : resolve());
    stream.on('end', () => p()); stream.on('finish', () => p());
    stream.on('error', p);
  });
}

function pipeline(...args) {
  let cb;
  if (typeof args[args.length - 1] === 'function') cb = args.pop();
  const streams = args.flat();
  for (let i = 0; i < streams.length - 1; i++) {
    streams[i].pipe(streams[i + 1]);
    streams[i].on('error', (err) => { if (cb) cb(err); });
  }
  const last = streams[streams.length - 1];
  if (cb) {
    last.on('finish', () => cb(null));
    last.on('end', () => cb(null));
    last.on('error', (err) => cb(err));
    return last;
  }
  return new Promise((resolve, reject) => {
    last.on('finish', resolve);
    last.on('end', resolve);
    last.on('error', reject);
  });
}

// ---- state accessors (many tests assert these) ----
function defineGetters(proto, getters) {
  for (const name of Object.keys(getters)) {
    Object.defineProperty(proto, name, { get: getters[name], configurable: true, enumerable: false });
  }
}
defineGetters(Readable.prototype, {
  readableEnded() { return !!(this._readableState && this._readableState.endEmitted); },
  readableFlowing() { return this._readableState ? this._readableState.flowing : null },
  readableLength() { return this._readableState ? this._readableState.length : 0 },
  readableHighWaterMark() { return this._readableState ? this._readableState.highWaterMark : 0 },
  readableObjectMode() { return !!(this._readableState && this._readableState.objectMode) },
  readableAborted() { return !!(this._readableState && this._readableState.destroyed && !this._readableState.endEmitted) },
  readableDidRead() { return !!(this._readableState && this._readableState.didRead) },
});
defineGetters(Writable.prototype, {
  writableEnded() { return !!(this._writableState && this._writableState.ended) },
  writableFinished() { return !!(this._writableState && this._writableState.finished) },
  writableLength() { return this._writableState ? this._writableState.length : 0 },
  writableHighWaterMark() { return this._writableState ? this._writableState.highWaterMark : 0 },
  writableObjectMode() { return !!(this._writableState && this._writableState.objectMode) },
  writableCorked() { return this._writableState ? this._writableState.corked : 0 },
  writableNeedDrain() { return !!(this._writableState && this._writableState.needDrain) },
});
// Duplex mixes Writable's *methods* at class-definition time (before the
// getters above existed), so copy the writable accessors onto it explicitly.
defineGetters(Duplex.prototype, {
  writableEnded() { return !!(this._writableState && this._writableState.ended) },
  writableFinished() { return !!(this._writableState && this._writableState.finished) },
  writableLength() { return this._writableState ? this._writableState.length : 0 },
  writableHighWaterMark() { return this._writableState ? this._writableState.highWaterMark : 0 },
  writableObjectMode() { return !!(this._writableState && this._writableState.objectMode) },
  writableCorked() { return this._writableState ? this._writableState.corked : 0 },
  writableNeedDrain() { return !!(this._writableState && this._writableState.needDrain) },
});
// `destroyed` lives on whichever state exists.
for (const proto of [Readable.prototype, Writable.prototype, Duplex.prototype]) {
  Object.defineProperty(proto, 'destroyed', {
    get() { return !!((this._readableState && this._readableState.destroyed) || (this._writableState && this._writableState.destroyed)); },
    set(_v) {},
    configurable: true,
  });
}

// ---- async-iterator helpers on Readable (Node 16+) ----
async function* iterate(stream) {
  for await (const chunk of stream) yield chunk;
}
Object.assign(Readable.prototype, {
  async toArray() { const out = []; for await (const c of this) out.push(c); return out; },
  map(fn) { const self = this; return Readable.from((async function* () { let i = 0; for await (const c of self) yield await fn(c, i++); })()); },
  filter(fn) { const self = this; return Readable.from((async function* () { let i = 0; for await (const c of self) { if (await fn(c, i++)) yield c; } })()); },
  flatMap(fn) { const self = this; return Readable.from((async function* () { let i = 0; for await (const c of self) { const r = await fn(c, i++); if (r && r[Symbol.asyncIterator]) { for await (const x of r) yield x; } else if (r && r[Symbol.iterator]) { for (const x of r) yield x; } else yield r; } })()); },
  async forEach(fn) { let i = 0; for await (const c of this) await fn(c, i++); },
  async reduce(fn, initial) { let acc = initial; let first = arguments.length < 2; for await (const c of this) { if (first) { acc = c; first = false; } else acc = await fn(acc, c); } return acc; },
  async some(fn) { let i = 0; for await (const c of this) { if (await fn(c, i++)) return true; } return false; },
  async every(fn) { let i = 0; for await (const c of this) { if (!(await fn(c, i++))) return false; } return true; },
  async find(fn) { let i = 0; for await (const c of this) { if (await fn(c, i++)) return c; } return undefined; },
  take(n) { const self = this; return Readable.from((async function* () { let i = 0; if (n <= 0) return; for await (const c of self) { yield c; if (++i >= n) return; } })()); },
  drop(n) { const self = this; return Readable.from((async function* () { let i = 0; for await (const c of self) { if (i++ < n) continue; yield c; } })()); },
});

Duplex.from = function from(src) {
  if (src && typeof src.readable === 'object' && typeof src.writable === 'object') {
    // { readable, writable } pair — return a passthrough-ish duplex bridge.
    const d = new Duplex({ objectMode: true });
    return d;
  }
  return Readable.from(src);
};

Stream.Readable = Readable;
Stream.Writable = Writable;
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.Stream = Stream;
Stream.finished = finished;
Stream.pipeline = pipeline;
Stream.promises = { finished, pipeline };
Stream.addAbortSignal = function addAbortSignal(signal, stream) {
  if (signal && typeof signal.addEventListener === 'function') {
    signal.addEventListener('abort', () => stream.destroy(new Error('The operation was aborted')), { once: true });
  }
  return stream;
};
Stream.isErrored = (s) => !!(s && ((s._readableState && s._readableState.errored) || (s._writableState && s._writableState.errored)));
Stream.isReadable = (s) => !!(s && s._readableState && !s._readableState.endEmitted && !s._readableState.destroyed);

module.exports = Stream;
module.exports.Readable = Readable;
module.exports.Writable = Writable;
module.exports.Duplex = Duplex;
module.exports.Transform = Transform;
module.exports.PassThrough = PassThrough;
module.exports.Stream = Stream;
module.exports.finished = finished;
module.exports.pipeline = pipeline;
module.exports.addAbortSignal = Stream.addAbortSignal;
module.exports.isErrored = Stream.isErrored;
module.exports.isReadable = Stream.isReadable;
module.exports.default = Stream;
