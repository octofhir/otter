'use strict';
// WHATWG Streams — a practical implementation of ReadableStream /
// WritableStream / TransformStream and the encoding transform streams, over the
// existing Promise/queueMicrotask intrinsics. Default (non-byte) streams with
// start/pull/cancel and write/close/abort underlying source/sink hooks,
// backpressure via the writer `ready` promise, async iteration, tee, pipeTo /
// pipeThrough. Installed once at runtime bootstrap.

(function installWebStreams(global) {
  'use strict';

  function def(name, value) {
    Object.defineProperty(global, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  function deferred() {
    let resolve, reject;
    const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
    return { promise, resolve, reject };
  }

  function sizeOf(strategy, chunk) {
    if (strategy && typeof strategy.size === 'function') {
      const n = Number(strategy.size(chunk));
      return Number.isFinite(n) && n >= 0 ? n : 0;
    }
    return 1;
  }

  // ---- ReadableStream ----
  const R = {
    stream: Symbol('stream'),
    state: Symbol('state'),
    queue: Symbol('queue'),
    queueSize: Symbol('queueSize'),
    controller: Symbol('controller'),
    reader: Symbol('reader'),
    storedError: Symbol('storedError'),
    pullPromise: Symbol('pullPromise'),
    closeRequested: Symbol('closeRequested'),
    source: Symbol('source'),
    hwm: Symbol('hwm'),
    strategy: Symbol('strategy'),
    readRequests: Symbol('readRequests'),
    started: Symbol('started'),
  };

  class ReadableStreamDefaultController {
    constructor(stream) { this[R.stream] = stream; }
    get desiredSize() {
      const s = this[R.stream];
      if (s[R.state] === 'errored') return null;
      if (s[R.state] === 'closed') return 0;
      return s[R.hwm] - s[R.queueSize];
    }
    enqueue(chunk) {
      const s = this[R.stream];
      if (s[R.state] !== 'readable') throw new TypeError('Cannot enqueue: stream is not readable');
      if (s[R.readRequests].length > 0) {
        const req = s[R.readRequests].shift();
        req.resolve({ value: chunk, done: false });
      } else {
        s[R.queue].push(chunk);
        s[R.queueSize] += sizeOf(s[R.strategy], chunk);
      }
      pullIfNeeded(s);
    }
    close() {
      const s = this[R.stream];
      if (s[R.state] !== 'readable') return;
      s[R.closeRequested] = true;
      if (s[R.queue].length === 0) closeStream(s);
    }
    error(e) { errorStream(this[R.stream], e); }
  }

  function closeStream(s) {
    s[R.state] = 'closed';
    for (const req of s[R.readRequests].splice(0)) req.resolve({ value: undefined, done: true });
    if (s[R.reader] && s[R.reader]._closedDeferred) s[R.reader]._closedDeferred.resolve(undefined);
  }

  function errorStream(s, e) {
    if (s[R.state] !== 'readable') return;
    s[R.state] = 'errored';
    s[R.storedError] = e;
    s[R.queue].length = 0;
    s[R.queueSize] = 0;
    for (const req of s[R.readRequests].splice(0)) req.reject(e);
    if (s[R.reader] && s[R.reader]._closedDeferred) s[R.reader]._closedDeferred.reject(e);
  }

  function pullIfNeeded(s) {
    if (!s[R.started] || s[R.state] !== 'readable' || s[R.pullPromise]) return;
    const desired = s[R.hwm] - s[R.queueSize];
    if (desired <= 0 && s[R.readRequests].length === 0) return;
    const source = s[R.source];
    if (!source || typeof source.pull !== 'function') return;
    let result;
    try { result = source.pull(s[R.controller]); } catch (err) { errorStream(s, err); return; }
    s[R.pullPromise] = Promise.resolve(result).then(
      () => { s[R.pullPromise] = null; pullIfNeeded(s); },
      (err) => { s[R.pullPromise] = null; errorStream(s, err); });
  }

  class ReadableStreamDefaultReader {
    constructor(stream) {
      if (!(stream instanceof ReadableStream)) throw new TypeError('Not a ReadableStream');
      if (stream[R.reader]) throw new TypeError('ReadableStream is already locked');
      this[R.stream] = stream;
      stream[R.reader] = this;
      this._closedDeferred = deferred();
      if (stream[R.state] === 'closed') this._closedDeferred.resolve(undefined);
      else if (stream[R.state] === 'errored') this._closedDeferred.reject(stream[R.storedError]);
    }
    get closed() { return this._closedDeferred ? this._closedDeferred.promise : Promise.reject(new TypeError('released')); }
    read() {
      const s = this[R.stream];
      if (!s) return Promise.reject(new TypeError('Reader released'));
      if (s[R.queue].length > 0) {
        const chunk = s[R.queue].shift();
        s[R.queueSize] -= sizeOf(s[R.strategy], chunk);
        if (s[R.closeRequested] && s[R.queue].length === 0) closeStream(s);
        else pullIfNeeded(s);
        return Promise.resolve({ value: chunk, done: false });
      }
      if (s[R.state] === 'closed') return Promise.resolve({ value: undefined, done: true });
      if (s[R.state] === 'errored') return Promise.reject(s[R.storedError]);
      const req = deferred();
      s[R.readRequests].push(req);
      pullIfNeeded(s);
      return req.promise;
    }
    cancel(reason) {
      const s = this[R.stream];
      if (!s) return Promise.reject(new TypeError('Reader released'));
      return cancelStream(s, reason);
    }
    releaseLock() {
      const s = this[R.stream];
      if (!s) return;
      if (s[R.readRequests].length > 0) throw new TypeError('Cannot release a reader with pending reads');
      s[R.reader] = null;
      this[R.stream] = null;
      this._closedDeferred = null;
    }
  }

  function cancelStream(s, reason) {
    if (s[R.state] === 'closed') return Promise.resolve(undefined);
    if (s[R.state] === 'errored') return Promise.reject(s[R.storedError]);
    s[R.queue].length = 0;
    s[R.queueSize] = 0;
    const source = s[R.source];
    let result;
    try { result = source && typeof source.cancel === 'function' ? source.cancel(reason) : undefined; }
    catch (err) { return Promise.reject(err); }
    closeStream(s);
    return Promise.resolve(result).then(() => undefined);
  }

  class ReadableStream {
    constructor(underlyingSource = {}, strategy = {}) {
      const source = underlyingSource || {};
      this[R.source] = source;
      this[R.state] = 'readable';
      this[R.queue] = [];
      this[R.queueSize] = 0;
      this[R.readRequests] = [];
      this[R.closeRequested] = false;
      this[R.pullPromise] = null;
      this[R.reader] = null;
      this[R.storedError] = undefined;
      this[R.strategy] = strategy || {};
      this[R.hwm] = strategy && strategy.highWaterMark !== undefined ? Number(strategy.highWaterMark) : 1;
      this[R.started] = false;
      this[R.controller] = new ReadableStreamDefaultController(this);
      const startResult = typeof source.start === 'function'
        ? Promise.resolve().then(() => source.start(this[R.controller]))
        : Promise.resolve();
      startResult.then(
        () => { this[R.started] = true; pullIfNeeded(this); },
        (err) => errorStream(this, err));
    }
    get locked() { return this[R.reader] !== null; }
    getReader(options) {
      if (options && options.mode === 'byob') throw new TypeError('byob reader not supported');
      return new ReadableStreamDefaultReader(this);
    }
    cancel(reason) {
      if (this.locked) return Promise.reject(new TypeError('Cannot cancel a locked stream'));
      return cancelStream(this, reason);
    }
    [Symbol.asyncIterator](options) {
      const reader = this.getReader();
      const preventCancel = !!(options && options.preventCancel);
      return {
        next() {
          return reader.read().then((r) => {
            if (r.done) reader.releaseLock();
            return r;
          });
        },
        return(value) {
          if (!preventCancel) reader.cancel(value);
          reader.releaseLock();
          return Promise.resolve({ value, done: true });
        },
        [Symbol.asyncIterator]() { return this; },
      };
    }
    values(options) { return this[Symbol.asyncIterator](options); }
    tee() {
      const reader = this.getReader();
      let closed = false;
      const make = () => new ReadableStream({
        pull(controller) {
          return reader.read().then((r) => {
            if (r.done) { if (!closed) { closed = true; } controller.close(); return; }
            controller.enqueue(r.value);
          });
        },
        cancel(reason) { return reader.cancel(reason); },
      });
      return [make(), make()];
    }
    pipeTo(dest, options) {
      const reader = this.getReader();
      const writer = dest.getWriter();
      const preventCancel = !!(options && options.preventCancel);
      const preventClose = !!(options && options.preventClose);
      return new Promise((resolve, reject) => {
        const pump = () => reader.read().then((r) => {
          if (r.done) {
            reader.releaseLock();
            const fin = preventClose ? Promise.resolve() : writer.close();
            return fin.then(() => { writer.releaseLock(); resolve(undefined); });
          }
          return writer.write(r.value).then(pump);
        });
        pump().catch((err) => {
          reader.releaseLock();
          if (!preventCancel) reader.cancel(err);
          writer.abort(err);
          reject(err);
        });
      });
    }
    pipeThrough(transform, options) {
      this.pipeTo(transform.writable, options).catch(() => {});
      return transform.readable;
    }
  }
  Object.defineProperty(ReadableStream.prototype, Symbol.toStringTag,
    { value: 'ReadableStream', configurable: true });
  def('ReadableStream', ReadableStream);
  def('ReadableStreamDefaultReader', ReadableStreamDefaultReader);
  def('ReadableStreamDefaultController', ReadableStreamDefaultController);

  // ---- WritableStream ----
  const W = {
    state: Symbol('wstate'),
    sink: Symbol('sink'),
    controller: Symbol('wcontroller'),
    writer: Symbol('writer'),
    storedError: Symbol('werror'),
    queue: Symbol('wqueue'),
    inflight: Symbol('inflight'),
    started: Symbol('wstarted'),
    closeDeferred: Symbol('closeDeferred'),
  };

  class WritableStreamDefaultController {
    constructor(stream) { this[W.state] = stream; }
    error(e) { errorWritable(this[W.state], e); }
    get signal() { return undefined; }
  }

  function errorWritable(s, e) {
    if (s[W.state] === 'errored' || s[W.state] === 'closed') return;
    s[W.state] = 'errored';
    s[W.storedError] = e;
    for (const item of s[W.queue].splice(0)) item.reject(e);
    if (s[W.closeDeferred]) s[W.closeDeferred].reject(e);
  }

  function processWrites(s) {
    if (s[W.inflight] || !s[W.started]) return;
    if (s[W.queue].length === 0) return;
    const item = s[W.queue].shift();
    s[W.inflight] = true;
    let result;
    try {
      result = typeof s[W.sink].write === 'function'
        ? s[W.sink].write(item.chunk, s[W.controller]) : undefined;
    } catch (err) { s[W.inflight] = false; errorWritable(s, err); item.reject(err); return; }
    Promise.resolve(result).then(
      () => { s[W.inflight] = false; item.resolve(undefined); processWrites(s); },
      (err) => { s[W.inflight] = false; errorWritable(s, err); item.reject(err); });
  }

  class WritableStreamDefaultWriter {
    constructor(stream) {
      if (stream[W.writer]) throw new TypeError('WritableStream is already locked');
      this[W.state] = stream;
      stream[W.writer] = this;
      this._closedDeferred = deferred();
      this._readyDeferred = deferred();
      this._readyDeferred.resolve(undefined);
      if (stream[W.state] === 'errored') {
        this._closedDeferred.reject(stream[W.storedError]);
      }
    }
    get closed() { return this._closedDeferred.promise; }
    get ready() { return this._readyDeferred.promise; }
    get desiredSize() {
      const s = this[W.state];
      return s[W.state] === 'errored' ? null : (s[W.state] === 'closed' ? 0 : 1);
    }
    write(chunk) {
      const s = this[W.state];
      if (!s) return Promise.reject(new TypeError('Writer released'));
      if (s[W.state] === 'errored') return Promise.reject(s[W.storedError]);
      if (s[W.state] !== 'writable') return Promise.reject(new TypeError('Stream is not writable'));
      const item = deferred();
      item.chunk = chunk;
      s[W.queue].push(item);
      processWrites(s);
      return item.promise;
    }
    close() {
      const s = this[W.state];
      if (!s) return Promise.reject(new TypeError('Writer released'));
      return closeWritable(s);
    }
    abort(reason) {
      const s = this[W.state];
      if (!s) return Promise.reject(new TypeError('Writer released'));
      return abortWritable(s, reason);
    }
    releaseLock() {
      const s = this[W.state];
      if (!s) return;
      s[W.writer] = null;
      this[W.state] = null;
    }
  }

  function closeWritable(s) {
    if (s[W.state] === 'closed') return Promise.resolve(undefined);
    if (s[W.state] === 'errored') return Promise.reject(s[W.storedError]);
    s[W.closeDeferred] = s[W.closeDeferred] || deferred();
    const finish = () => {
      let result;
      try { result = typeof s[W.sink].close === 'function' ? s[W.sink].close() : undefined; }
      catch (err) { errorWritable(s, err); return; }
      Promise.resolve(result).then(
        () => { s[W.state] = 'closed'; s[W.closeDeferred].resolve(undefined);
          if (s[W.writer]) s[W.writer]._closedDeferred.resolve(undefined); },
        (err) => errorWritable(s, err));
    };
    const waitQueue = () => {
      if (s[W.queue].length === 0 && !s[W.inflight]) finish();
      else queueMicrotask(waitQueue);
    };
    waitQueue();
    return s[W.closeDeferred].promise;
  }

  function abortWritable(s, reason) {
    if (s[W.state] === 'closed' || s[W.state] === 'errored') return Promise.resolve(undefined);
    let result;
    try { result = typeof s[W.sink].abort === 'function' ? s[W.sink].abort(reason) : undefined; }
    catch (err) { errorWritable(s, err); return Promise.reject(err); }
    errorWritable(s, reason);
    return Promise.resolve(result).then(() => undefined);
  }

  class WritableStream {
    constructor(underlyingSink = {}, strategy = {}) {
      void strategy;
      const sink = underlyingSink || {};
      this[W.sink] = sink;
      this[W.state] = 'writable';
      this[W.queue] = [];
      this[W.inflight] = false;
      this[W.writer] = null;
      this[W.storedError] = undefined;
      this[W.closeDeferred] = null;
      this[W.started] = false;
      this[W.controller] = new WritableStreamDefaultController(this);
      const startResult = typeof sink.start === 'function'
        ? Promise.resolve().then(() => sink.start(this[W.controller]))
        : Promise.resolve();
      startResult.then(
        () => { this[W.started] = true; processWrites(this); },
        (err) => errorWritable(this, err));
    }
    get locked() { return this[W.writer] !== null; }
    getWriter() { return new WritableStreamDefaultWriter(this); }
    close() {
      if (this.locked) return Promise.reject(new TypeError('Cannot close a locked stream'));
      return closeWritable(this);
    }
    abort(reason) {
      if (this.locked) return Promise.reject(new TypeError('Cannot abort a locked stream'));
      return abortWritable(this, reason);
    }
  }
  Object.defineProperty(WritableStream.prototype, Symbol.toStringTag,
    { value: 'WritableStream', configurable: true });
  def('WritableStream', WritableStream);
  def('WritableStreamDefaultWriter', WritableStreamDefaultWriter);
  def('WritableStreamDefaultController', WritableStreamDefaultController);

  // ---- TransformStream ----
  class TransformStream {
    constructor(transformer = {}, writableStrategy = {}, readableStrategy = {}) {
      const t = transformer || {};
      let readableController;
      this.readable = new ReadableStream({
        start(controller) { readableController = controller; },
        cancel() {},
      }, readableStrategy);
      const enqueue = (chunk) => readableController.enqueue(chunk);
      const transformController = {
        enqueue,
        terminate() { try { readableController.close(); } catch (_) {} },
        error(e) { readableController.error(e); },
        get desiredSize() { return readableController.desiredSize; },
      };
      this.writable = new WritableStream({
        start() {
          if (typeof t.start === 'function') return t.start(transformController);
        },
        write(chunk) {
          if (typeof t.transform === 'function') return t.transform(chunk, transformController);
          enqueue(chunk);
        },
        close() {
          const flush = typeof t.flush === 'function'
            ? Promise.resolve(t.flush(transformController)) : Promise.resolve();
          return flush.then(() => { try { readableController.close(); } catch (_) {} });
        },
        abort(reason) { readableController.error(reason); },
      }, writableStrategy);
    }
  }
  Object.defineProperty(TransformStream.prototype, Symbol.toStringTag,
    { value: 'TransformStream', configurable: true });
  def('TransformStream', TransformStream);

  // ---- TextEncoderStream / TextDecoderStream ----
  class TextEncoderStream extends TransformStream {
    constructor() {
      const encoder = new TextEncoder();
      super({
        transform(chunk, controller) { controller.enqueue(encoder.encode(String(chunk))); },
      });
      Object.defineProperty(this, 'encoding', { value: 'utf-8', enumerable: true });
    }
  }
  Object.defineProperty(TextEncoderStream.prototype, Symbol.toStringTag,
    { value: 'TextEncoderStream', configurable: true });
  def('TextEncoderStream', TextEncoderStream);

  class TextDecoderStream extends TransformStream {
    constructor(label = 'utf-8', options = {}) {
      const decoder = new TextDecoder(label, options);
      super({
        transform(chunk, controller) {
          const text = decoder.decode(chunk);
          if (text) controller.enqueue(text);
        },
        flush(controller) {
          const text = decoder.decode();
          if (text) controller.enqueue(text);
        },
      });
      Object.defineProperty(this, 'encoding', { value: decoder.encoding, enumerable: true });
    }
  }
  Object.defineProperty(TextDecoderStream.prototype, Symbol.toStringTag,
    { value: 'TextDecoderStream', configurable: true });
  def('TextDecoderStream', TextDecoderStream);
})(globalThis);
