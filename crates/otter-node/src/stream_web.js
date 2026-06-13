'use strict';
// `node:stream/web` — a practical subset of the WHATWG Streams API
// (ReadableStream / WritableStream / TransformStream + queuing strategies).
// Dependency-free; queue-and-promise based rather than spec-byte-exact.

function makeDeferred() {
  let resolve; let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

class ReadableStreamDefaultController {
  constructor(stream) { this._stream = stream; }
  enqueue(chunk) {
    const s = this._stream;
    if (s._waiters.length) s._waiters.shift().resolve({ value: chunk, done: false });
    else s._queue.push(chunk);
  }
  close() {
    const s = this._stream;
    s._closed = true;
    while (s._waiters.length) s._waiters.shift().resolve({ value: undefined, done: true });
  }
  error(e) {
    const s = this._stream;
    s._errored = e;
    while (s._waiters.length) s._waiters.shift().reject(e);
  }
  get desiredSize() { return 1; }
}

class ReadableStreamDefaultReader {
  constructor(stream) {
    this._stream = stream;
    this._closedDeferred = makeDeferred();
    if (stream._closed) this._closedDeferred.resolve();
  }
  read() {
    const s = this._stream;
    if (s._errored) return Promise.reject(s._errored);
    if (s._queue.length) return Promise.resolve({ value: s._queue.shift(), done: false });
    if (s._closed) return Promise.resolve({ value: undefined, done: true });
    const d = makeDeferred();
    s._waiters.push(d);
    if (typeof s._pull === 'function') Promise.resolve().then(() => s._pull(s._controller));
    return d.promise;
  }
  get closed() { return this._closedDeferred.promise; }
  releaseLock() { this._stream._locked = false; }
  cancel(reason) { return this._stream.cancel(reason); }
}

class ReadableStream {
  constructor(underlyingSource = {}, strategy = {}) {
    this._queue = [];
    this._waiters = [];
    this._closed = false;
    this._errored = null;
    this._locked = false;
    this._controller = new ReadableStreamDefaultController(this);
    this._pull = underlyingSource.pull;
    this._cancelFn = underlyingSource.cancel;
    if (typeof underlyingSource.start === 'function') {
      Promise.resolve(underlyingSource.start(this._controller)).catch((e) => this._controller.error(e));
    }
  }

  get locked() { return this._locked; }

  getReader(options) {
    if (options && options.mode === 'byob') throw new TypeError('BYOB readers are not supported.');
    this._locked = true;
    return new ReadableStreamDefaultReader(this);
  }

  cancel(reason) {
    this._closed = true;
    while (this._waiters.length) this._waiters.shift().resolve({ value: undefined, done: true });
    if (typeof this._cancelFn === 'function') return Promise.resolve(this._cancelFn(reason));
    return Promise.resolve();
  }

  async pipeTo(dest, options) {
    const reader = this.getReader();
    const writer = dest.getWriter();
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        await writer.write(value);
      }
      await writer.close();
    } catch (err) {
      if (writer.abort) await writer.abort(err);
      throw err;
    } finally {
      reader.releaseLock();
    }
  }

  pipeThrough(transform, options) {
    this.pipeTo(transform.writable, options).catch(() => {});
    return transform.readable;
  }

  tee() {
    const chunks = [];
    const branch = () => new ReadableStream({
      start: async (controller) => {
        const reader = this.getReader();
        while (true) {
          const { value, done } = await reader.read();
          if (done) { controller.close(); break; }
          controller.enqueue(value);
        }
      },
    });
    return [branch(), branch()];
  }

  [Symbol.asyncIterator]() {
    const reader = this.getReader();
    return {
      next() { return reader.read(); },
      return() { reader.releaseLock(); return Promise.resolve({ value: undefined, done: true }); },
      [Symbol.asyncIterator]() { return this; },
    };
  }

  static from(iterable) {
    const it = iterable[Symbol.asyncIterator] ? iterable[Symbol.asyncIterator]() : iterable[Symbol.iterator]();
    return new ReadableStream({
      async pull(controller) {
        const { value, done } = await it.next();
        if (done) controller.close(); else controller.enqueue(value);
      },
    });
  }
}

class WritableStreamDefaultWriter {
  constructor(stream) {
    this._stream = stream;
    this._readyDeferred = makeDeferred();
    this._readyDeferred.resolve();
    this._closedDeferred = makeDeferred();
  }
  get ready() { return this._readyDeferred.promise; }
  get closed() { return this._closedDeferred.promise; }
  get desiredSize() { return 1; }
  write(chunk) {
    const s = this._stream;
    if (s._errored) return Promise.reject(s._errored);
    return Promise.resolve(s._write ? s._write(chunk, s._controller) : undefined);
  }
  close() {
    const s = this._stream;
    return Promise.resolve(s._close ? s._close() : undefined).then(() => {
      s._closed = true;
      this._closedDeferred.resolve();
    });
  }
  abort(reason) {
    const s = this._stream;
    s._errored = reason || new Error('aborted');
    return Promise.resolve(s._abort ? s._abort(reason) : undefined);
  }
  releaseLock() { this._stream._locked = false; }
}

class WritableStreamDefaultController {
  constructor(stream) { this._stream = stream; }
  error(e) { this._stream._errored = e; }
  get signal() { return undefined; }
}

class WritableStream {
  constructor(underlyingSink = {}, strategy = {}) {
    this._closed = false;
    this._errored = null;
    this._locked = false;
    this._controller = new WritableStreamDefaultController(this);
    this._write = underlyingSink.write;
    this._close = underlyingSink.close;
    this._abort = underlyingSink.abort;
    if (typeof underlyingSink.start === 'function') {
      Promise.resolve(underlyingSink.start(this._controller)).catch((e) => { this._errored = e; });
    }
  }
  get locked() { return this._locked; }
  getWriter() { this._locked = true; return new WritableStreamDefaultWriter(this); }
  close() { return this.getWriter().close(); }
  abort(reason) { return this.getWriter().abort(reason); }
}

class TransformStreamDefaultController {
  constructor(transform) { this._transform = transform; }
  enqueue(chunk) { this._transform._readableController.enqueue(chunk); }
  terminate() { this._transform._readableController.close(); }
  error(e) {
    this._transform._readableController.error(e);
  }
  get desiredSize() { return 1; }
}

class TransformStream {
  constructor(transformer = {}, writableStrategy = {}, readableStrategy = {}) {
    const self = this;
    this.readable = new ReadableStream({
      start(controller) { self._readableController = controller; },
    });
    this._controller = new TransformStreamDefaultController(this);
    const transformFn = transformer.transform || ((chunk, controller) => controller.enqueue(chunk));
    const flushFn = transformer.flush;
    this.writable = new WritableStream({
      write(chunk) { return Promise.resolve(transformFn(chunk, self._controller)); },
      close() {
        return Promise.resolve(flushFn ? flushFn(self._controller) : undefined)
          .then(() => self._readableController.close());
      },
      abort(reason) { self._readableController.error(reason); },
    });
    if (typeof transformer.start === 'function') transformer.start(this._controller);
  }
}

class CountQueuingStrategy {
  constructor(opts) { this.highWaterMark = opts.highWaterMark; }
  size() { return 1; }
}
class ByteLengthQueuingStrategy {
  constructor(opts) { this.highWaterMark = opts.highWaterMark; }
  size(chunk) { return chunk.byteLength; }
}

module.exports = {
  ReadableStream,
  ReadableStreamDefaultReader,
  ReadableStreamDefaultController,
  WritableStream,
  WritableStreamDefaultWriter,
  WritableStreamDefaultController,
  TransformStream,
  TransformStreamDefaultController,
  CountQueuingStrategy,
  ByteLengthQueuingStrategy,
};
