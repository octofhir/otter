'use strict';

// internalBinding('zlib') — the handle classes vendored `zlib.js` drives.
//
// Node's C++ binding is a per-stream engine plus two counters the JS side
// reads out of a shared `Uint32Array`: how much room is left in the output
// buffer and how much input went unread. This file is that contract over
// the host engine in `internal/otter/zlib`; the work itself is done inside
// `process`, and only the completion callback is deferred, which is what
// makes `write` asynchronous and `writeSync` not.

const native = require('internal/otter/zlib');

// Which counter is which, as `_processChunk` reads them.
const kAvailOutAfter = 0;
const kAvailInAfter = 1;

class Zlib {
  constructor(mode) {
    this.mode = mode;
    this._id = native.create(mode);
    this._writeState = null;
    this._callback = null;
    this.onerror = null;
    this.buffer = null;
    this.cb = null;
    this.availOutBefore = 0;
    this.availInBefore = 0;
    this.inOff = 0;
    this.flushFlag = 0;
  }

  init(windowBits, level, memLevel, strategy, writeState, processCallback, dictionary) {
    this._writeState = writeState;
    this._callback = processCallback;
    native.init(this._id, windowBits, level, memLevel, strategy, dictionary);
  }

  _run(flush, inBuf, inOff, inLen, outBuf, outOff, outLen) {
    const result = native.process(
      this._id, flush, inBuf, inOff, inLen, outBuf, outOff, outLen);
    if (result.error !== undefined) {
      if (typeof this.onerror === 'function') {
        // The binding reports the message and zlib's own Z_DATA_ERROR.
        this.onerror(result.error, -3, 'Z_DATA_ERROR');
      }
      return false;
    }
    const state = this._writeState;
    if (state !== null) {
      state[kAvailOutAfter] = result.availOutAfter;
      state[kAvailInAfter] = result.availInAfter;
    }
    return true;
  }

  write(flush, inBuf, inOff, inLen, outBuf, outOff, outLen) {
    const handle = this;
    // The engine runs now; only the completion is deferred, so a stream
    // that writes in a loop still yields to the event loop between chunks.
    const ok = this._run(flush, inBuf, inOff, inLen, outBuf, outOff, outLen);
    setImmediate(() => {
      if (!ok) return;
      if (typeof handle._callback === 'function') {
        Reflect.apply(handle._callback, handle, []);
      }
    });
  }

  writeSync(flush, inBuf, inOff, inLen, outBuf, outOff, outLen) {
    this._run(flush, inBuf, inOff, inLen, outBuf, outOff, outLen);
  }

  params(level, strategy) {
    native.params(this._id, level, strategy);
  }

  reset() {
    native.reset(this._id);
  }

  close() {
    native.close(this._id);
    this._writeState = null;
    this._callback = null;
  }
}

// Brotli and Zstd are separate engines in Node's binding, driven through
// the same handle contract: the parameters arrive as a key/value array
// instead of positional arguments, and the flush value is the codec's own
// operation rather than a zlib one.
// Both codecs take their settings as an array indexed by parameter id,
// with `-1` meaning "leave the default".
const BROTLI_PARAM_QUALITY = 1;
const BROTLI_PARAM_LGWIN = 2;
const ZSTD_C_COMPRESSION_LEVEL = 100;

function settingOf(params, index) {
  const value = params?.[index];
  return typeof value === 'number' && value !== -1 ? value : -1;
}

class BrotliHandle extends Zlib {
  init(params, writeState, processCallback, dictionary) {
    this._writeState = writeState;
    this._callback = processCallback;
    native.init(
      this._id,
      settingOf(params, BROTLI_PARAM_LGWIN),
      settingOf(params, BROTLI_PARAM_QUALITY),
      8,
      0,
      dictionary,
    );
  }
}

class ZstdHandle extends Zlib {
  init(params, pledgedSrcSize, writeState, processCallback, dictionary) {
    this._writeState = writeState;
    this._callback = processCallback;
    native.init(
      this._id,
      0,
      settingOf(params, ZSTD_C_COMPRESSION_LEVEL),
      8,
      0,
      dictionary,
    );
  }
}

class BrotliEncoder extends BrotliHandle {
  constructor() { super(9); }
}
class BrotliDecoder extends BrotliHandle {
  constructor() { super(8); }
}
class ZstdCompress extends ZstdHandle {
  constructor() { super(10); }
}
class ZstdDecompress extends ZstdHandle {
  constructor() { super(11); }
}

module.exports = {
  Zlib,
  BrotliEncoder,
  BrotliDecoder,
  ZstdCompress,
  ZstdDecompress,
  crc32(data, value = 0) {
    return native.crc32(data, value);
  },
};
