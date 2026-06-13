'use strict';
// node:zlib — JS surface over the native DEFLATE/zlib/gzip core (__zlibnative).
//
// What this emulates:
// - zlib.constants / zlib.codes (frozen, immutable) — Z_* flush/level/error ids.
// - One-shot *Sync codecs (deflate/inflate/gzip/gunzip/deflateRaw/inflateRaw/unzip)
//   and their async callback counterparts (defer via setTimeout(...,0) → cb).
// - Stream classes (Deflate/Inflate/Gzip/Gunzip/DeflateRaw/InflateRaw/Unzip) as
//   Transform streams that buffer all input and codec it on _flush. This is a
//   whole-buffer approximation of Node's incremental zlib streams — enough for
//   create/instanceof/round-trip coverage, not flush-boundary fidelity.
// - zlib.crc32(data, value).
//
// Bytes reach the native layer as latin1 strings (1 byte ↔ 1 char); see zlib.rs.
// Brotli/zstd are intentionally not implemented (separate native backend).

const native = require('__zlibnative');
const { Buffer } = require('buffer');
const { Transform } = require('stream');

// ---- error helpers (mirror lib/internal/errors ERR_INVALID_ARG_TYPE) --------

function invalidArgTypeSuffix(input) {
  if (input === undefined || input === null) return ` Received ${input}`;
  if (typeof input === 'function') return ` Received function ${input.name}`;
  if (typeof input === 'object') {
    if (input.constructor && input.constructor.name) {
      return ` Received an instance of ${input.constructor.name}`;
    }
    return ' Received an instance of Object';
  }
  return ` Received type ${typeof input} (${String(input)})`;
}

function argTypeError(name, expected, input) {
  const e = new TypeError(
    `The "${name}" argument must be ${expected}.` + invalidArgTypeSuffix(input)
  );
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

const BUFFER_EXPECTED =
  'of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer';

// Normalize a zlib input (string|Buffer|TypedArray|DataView|ArrayBuffer) into a
// Buffer, throwing the Node-exact ERR_INVALID_ARG_TYPE otherwise.
function toBuffer(input, name) {
  if (typeof input === 'string') return Buffer.from(input, 'utf8');
  if (Buffer.isBuffer(input)) return input;
  if (ArrayBuffer.isView(input)) {
    return Buffer.from(input.buffer, input.byteOffset, input.byteLength);
  }
  if (input instanceof ArrayBuffer) return Buffer.from(input);
  throw argTypeError(name || 'buffer', BUFFER_EXPECTED, input);
}

function levelOf(opts) {
  return opts && typeof opts.level === 'number' ? opts.level : -1;
}

// ---- constants & codes ------------------------------------------------------

const constants = {
  Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3,
  Z_FINISH: 4, Z_BLOCK: 5, Z_TREES: 6,
  Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2,
  Z_DATA_ERROR: -3, Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6,
  Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1, Z_BEST_COMPRESSION: 9,
  Z_DEFAULT_COMPRESSION: -1,
  Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3, Z_FIXED: 4, Z_DEFAULT_STRATEGY: 0,
  ZLIB_VERNUM: 0x12b0,
  DEFLATE: 1, INFLATE: 2, GZIP: 3, GUNZIP: 4, DEFLATERAW: 5, INFLATERAW: 6,
  UNZIP: 7,
  Z_MIN_WINDOWBITS: 8, Z_MAX_WINDOWBITS: 15, Z_DEFAULT_WINDOWBITS: 15,
  Z_MIN_CHUNK: 64, Z_MAX_CHUNK: Infinity, Z_DEFAULT_CHUNK: 16384,
  Z_MIN_MEMLEVEL: 1, Z_MAX_MEMLEVEL: 9, Z_DEFAULT_MEMLEVEL: 8,
  Z_MIN_LEVEL: -1, Z_MAX_LEVEL: 9, Z_DEFAULT_LEVEL: -1,
};
Object.freeze(constants);

// codes: bidirectional name<->number map (frozen), per Node's zlib.codes.
const codes = {
  Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2,
  Z_DATA_ERROR: -3, Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6,
};
for (const key of Object.keys(codes)) {
  codes[codes[key]] = key;
}
Object.freeze(codes);

// ---- sync codecs ------------------------------------------------------------

// info option → { buffer, engine } where engine instanceof the matching class.
function withInfo(result, opts, EngineClass) {
  if (opts && opts.info) {
    return { buffer: result, engine: new EngineClass(opts) };
  }
  return result;
}

function makeSync(nativeFn, EngineClass) {
  return function (input, opts) {
    const buf = toBuffer(input);
    const out = nativeFn(buf.toString('latin1'), levelOf(opts));
    return withInfo(Buffer.from(out, 'latin1'), opts, EngineClass);
  };
}

// Async callback codec; defers via setTimeout(0). Throws synchronously when the
// callback is missing/invalid (Node validates the callback before scheduling).
function makeAsync(syncFn) {
  return function (input, opts, cb) {
    if (typeof opts === 'function') {
      cb = opts;
      opts = {};
    }
    if (typeof cb !== 'function') {
      throw argTypeError('callback', 'of type function', cb);
    }
    setTimeout(() => {
      let result;
      try {
        result = syncFn(input, opts);
      } catch (err) {
        cb(err);
        return;
      }
      cb(null, result);
    }, 0);
  };
}

// ---- option validation (mirrors lib/zlib.js checkRangesOrGetDefault) --------

// Render a value for a "Received ..." type-error clause: strings are quoted,
// other primitives carry their (typeof, value), objects name their constructor.
function typeReceived(value) {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  const t = typeof value;
  if (t === 'string') return `type string ('${value}')`;
  if (t === 'object') {
    return value.constructor && value.constructor.name
      ? `an instance of ${value.constructor.name}`
      : 'an instance of Object';
  }
  return `type ${t} (${String(value)})`;
}

function invalidArgTypeProp(label, kind, expected, value) {
  const e = new TypeError(
    `The "${label}" ${kind} must be ${expected}. Received ${typeReceived(value)}`
  );
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

function outOfRange(label, constraint, value) {
  const e = new RangeError(
    `The value of "${label}" is out of range. It must ${constraint}. ` +
      `Received ${value}`
  );
  e.code = 'ERR_OUT_OF_RANGE';
  return e;
}

// Validate a numeric option in [min, max] (max omitted → lower-bound only).
function validateNumberOpt(opts, key, label, min, max) {
  if (opts[key] === undefined) return;
  const v = opts[key];
  if (typeof v !== 'number') {
    throw invalidArgTypeProp(label, 'property', 'of type number', v);
  }
  if (!Number.isFinite(v)) throw outOfRange(label, 'be a finite number', v);
  if (max === undefined) {
    if (v < min) throw outOfRange(label, `be >= ${min}`, v);
  } else if (v < min || v > max) {
    throw outOfRange(label, `be >= ${min} and <= ${max}`, v);
  }
}

// level/strategy are special: NaN coerces to the default instead of throwing.
function validateLevelLike(opts, key, label, min, max, deflt) {
  const v = opts[key];
  if (v === undefined) return deflt;
  if (typeof v !== 'number') {
    throw invalidArgTypeProp(label, 'property', 'of type number', v);
  }
  if (Number.isNaN(v)) return deflt;
  if (!Number.isFinite(v)) throw outOfRange(label, 'be a finite number', v);
  if (v < min || v > max) {
    throw outOfRange(label, `be >= ${min} and <= ${max}`, v);
  }
  return v;
}

const DICTIONARY_EXPECTED =
  'an instance of Buffer, TypedArray, DataView, or ArrayBuffer';

function validateOptions(opts, gzipFlavor) {
  validateNumberOpt(opts, 'chunkSize', 'options.chunkSize', 64);
  validateNumberOpt(opts, 'windowBits', 'options.windowBits', gzipFlavor ? 9 : 8, 15);
  validateNumberOpt(opts, 'memLevel', 'options.memLevel', 1, 9);
  validateNumberOpt(opts, 'flush', 'options.flush', 0, 5);
  validateNumberOpt(opts, 'finishFlush', 'options.finishFlush', 0, 5);
  if (opts.dictionary !== undefined) {
    const d = opts.dictionary;
    if (!Buffer.isBuffer(d) && !ArrayBuffer.isView(d) &&
        !(d instanceof ArrayBuffer)) {
      throw invalidArgTypeProp('options.dictionary', 'property',
        DICTIONARY_EXPECTED, d);
    }
  }
}

// ---- stream classes ---------------------------------------------------------

// Whole-buffer Transform: collect chunks, codec on _flush. objectMode writes of
// non-buffer/string values throw ERR_INVALID_ARG_TYPE like Node's zlib streams.
class ZlibBase extends Transform {
  constructor(opts, nativeFn, gzipFlavor) {
    const o = opts || {};
    validateOptions(o, gzipFlavor);
    super(o);
    this._zlibNative = nativeFn;
    this._zlibOpts = o;
    this._zlibChunks = [];
    this.bytesWritten = 0;
    // _level/_strategy mirror Node's resolved settings (NaN → default).
    this._level = validateLevelLike(o, 'level', 'options.level', -1, 9, -1);
    this._strategy =
      validateLevelLike(o, 'strategy', 'options.strategy', 0, 4, 0);
  }

  _transform(chunk, encoding, callback) {
    if (typeof chunk === 'string') {
      chunk = Buffer.from(chunk, encoding === 'buffer' ? undefined : encoding);
    } else if (!Buffer.isBuffer(chunk) && !ArrayBuffer.isView(chunk)) {
      callback(argTypeError('chunk', BUFFER_EXPECTED, chunk));
      return;
    } else if (!Buffer.isBuffer(chunk)) {
      chunk = Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    }
    this._zlibChunks.push(chunk);
    this.bytesWritten += chunk.length;
    callback();
  }

  _flush(callback) {
    let out;
    try {
      const input = Buffer.concat(this._zlibChunks);
      out = this._zlibNative(input.toString('latin1'), this._level);
    } catch (err) {
      callback(err);
      return;
    }
    this.push(Buffer.from(out, 'latin1'));
    callback();
  }

  // flush()/params()/reset()/close() exist so callers don't hit a missing
  // method; this whole-buffer codec has no incremental flush boundaries, so
  // these are lightweight (params still validates like Node).
  flush(kind, callback) {
    if (typeof kind === 'function') {
      callback = kind;
    }
    if (typeof callback === 'function') setTimeout(callback, 0);
    return this;
  }

  params(level, strategy, callback) {
    if (typeof level !== 'number') {
      throw invalidArgTypeProp('level', 'argument', 'of type number', level);
    }
    if (!Number.isFinite(level)) {
      throw outOfRange('level', 'be a finite number', level);
    }
    if (level < -1 || level > 9) {
      throw outOfRange('level', 'be >= -1 and <= 9', level);
    }
    if (strategy !== undefined) {
      if (typeof strategy !== 'number') {
        throw invalidArgTypeProp('strategy', 'argument', 'of type number',
          strategy);
      }
      if (!Number.isFinite(strategy)) {
        throw outOfRange('strategy', 'be a finite number', strategy);
      }
      if (strategy < 0 || strategy > 4) {
        throw outOfRange('strategy', 'be >= 0 and <= 4', strategy);
      }
      this._strategy = strategy;
    }
    this._level = level;
    if (typeof callback === 'function') setTimeout(callback, 0);
    return this;
  }

  reset() {
    return this;
  }

  close(callback) {
    if (typeof callback === 'function') setTimeout(callback, 0);
    this.destroy();
    return this;
  }
}

class Deflate extends ZlibBase {
  constructor(opts) { super(opts, native.deflate, false); }
}
class Inflate extends ZlibBase {
  constructor(opts) { super(opts, native.inflate, false); }
}
class Gzip extends ZlibBase {
  constructor(opts) { super(opts, native.gzip, true); }
}
class Gunzip extends ZlibBase {
  constructor(opts) { super(opts, native.gunzip, true); }
}
class DeflateRaw extends ZlibBase {
  constructor(opts) { super(opts, native.deflateRaw, false); }
}
class InflateRaw extends ZlibBase {
  constructor(opts) { super(opts, native.inflateRaw, false); }
}
class Unzip extends ZlibBase {
  constructor(opts) { super(opts, native.unzip, true); }
}

// Node lets the codec classes be invoked with OR without `new`. ES classes
// require `new`, so export a thin callable wrapper sharing the real prototype
// (keeps `x instanceof zlib.Deflate` working either way).
function callable(RealClass) {
  function Ctor(opts) {
    return new RealClass(opts);
  }
  // Share the real prototype so `x instanceof zlib.Deflate` holds whether the
  // class was invoked with or without `new`.
  Ctor.prototype = RealClass.prototype;
  return Ctor;
}

const DeflateC = callable(Deflate);
const InflateC = callable(Inflate);
const GzipC = callable(Gzip);
const GunzipC = callable(Gunzip);
const DeflateRawC = callable(DeflateRaw);
const InflateRawC = callable(InflateRaw);
const UnzipC = callable(Unzip);

// ---- crc32 ------------------------------------------------------------------

function crc32(data, value) {
  const buf = toBuffer(data, 'data');
  if (value === undefined) value = 0;
  if (typeof value !== 'number') {
    throw argTypeError('value', 'of type number', value);
  }
  return native.crc32(buf.toString('latin1'), value >>> 0);
}

// ---- exports ----------------------------------------------------------------

const exportsObj = {
  constants,
  codes,
  crc32,

  Deflate: DeflateC, Inflate: InflateC, Gzip: GzipC, Gunzip: GunzipC,
  DeflateRaw: DeflateRawC, InflateRaw: InflateRawC, Unzip: UnzipC,

  createDeflate: (o) => new Deflate(o),
  createInflate: (o) => new Inflate(o),
  createGzip: (o) => new Gzip(o),
  createGunzip: (o) => new Gunzip(o),
  createDeflateRaw: (o) => new DeflateRaw(o),
  createInflateRaw: (o) => new InflateRaw(o),
  createUnzip: (o) => new Unzip(o),

  deflateSync: makeSync(native.deflate, Deflate),
  inflateSync: makeSync(native.inflate, Inflate),
  gzipSync: makeSync(native.gzip, Gzip),
  gunzipSync: makeSync(native.gunzip, Gunzip),
  deflateRawSync: makeSync(native.deflateRaw, DeflateRaw),
  inflateRawSync: makeSync(native.inflateRaw, InflateRaw),
  unzipSync: makeSync(native.unzip, Unzip),
};

exportsObj.deflate = makeAsync(exportsObj.deflateSync);
exportsObj.inflate = makeAsync(exportsObj.inflateSync);
exportsObj.gzip = makeAsync(exportsObj.gzipSync);
exportsObj.gunzip = makeAsync(exportsObj.gunzipSync);
exportsObj.deflateRaw = makeAsync(exportsObj.deflateRawSync);
exportsObj.inflateRaw = makeAsync(exportsObj.inflateRawSync);
exportsObj.unzip = makeAsync(exportsObj.unzipSync);

// constants/codes are immutable top-level properties (zlib.codes = {...} throws).
Object.defineProperty(exportsObj, 'constants', {
  value: constants, writable: false, enumerable: true, configurable: false,
});
Object.defineProperty(exportsObj, 'codes', {
  value: codes, writable: false, enumerable: true, configurable: false,
});

module.exports = exportsObj;
