'use strict';
// `node:crypto` — hashing (SHA-2), HMAC, and CSPRNG helpers on the native core.
// Public-key / cipher operations are not implemented in this slice.

const native = require('__cryptonative');
const { Buffer } = require('buffer');
const EventEmitter = require('events');

function toLatin1(data, enc) {
  if (typeof data === 'string') return Buffer.from(data, enc || 'utf8').toString('latin1');
  if (Buffer.isBuffer(data)) return data.toString('latin1');
  if (data instanceof Uint8Array || (data && data.buffer instanceof ArrayBuffer)) {
    return Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString('latin1');
  }
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('latin1');
  return Buffer.from(String(data), 'utf8').toString('latin1');
}

// Hash/Hmac are legacy Transform-ish streams in Node: write/end feed data and
// read() returns the digest. We implement the synchronous subset (update/digest
// + write/end/read + pipe) on EventEmitter so both styles work.
class Hash extends EventEmitter {
  constructor(algorithm) { super(); this._algo = String(algorithm); this._chunks = []; this._done = false; this._digest = null; }
  update(data, inputEncoding) {
    if (this._done) throw new Error('Digest already called');
    this._chunks.push(toLatin1(data, inputEncoding));
    return this;
  }
  _compute() { return Buffer.from(native.hashDigest(this._algo, this._chunks.join('')), 'latin1'); }
  digest(encoding) {
    if (this._done) throw new Error('Digest already called');
    this._done = true;
    const buf = this._compute();
    return encoding && encoding !== 'buffer' ? buf.toString(encoding) : buf;
  }
  write(data, enc, cb) { this.update(data, typeof enc === 'string' ? enc : undefined); if (typeof enc === 'function') enc(); else if (cb) cb(); return true; }
  end(data, enc, cb) {
    if (data !== undefined && data !== null && typeof data !== 'function') this.update(data, typeof enc === 'string' ? enc : undefined);
    if (!this._done) { this._done = true; this._digest = this._compute(); }
    this.emit('finish');
    this.emit('readable');
    this.emit('end');
    const done = typeof data === 'function' ? data : typeof enc === 'function' ? enc : cb;
    if (typeof done === 'function') done();
    return this;
  }
  read() { if (!this._done) { this._done = true; this._digest = this._compute(); } const d = this._digest; this._digest = null; return d; }
  pipe(dest) { this.on('end', () => { if (this._digest) dest.write(this._digest); dest.end(); }); return dest; }
  setEncoding() { return this; }
  copy() { const h = new Hash(this._algo); h._chunks = this._chunks.slice(); return h; }
}

class Hmac extends EventEmitter {
  constructor(algorithm, key) {
    super();
    this._algo = String(algorithm);
    this._key = toLatin1(key && key.length !== undefined ? key : String(key));
    this._chunks = [];
    this._done = false;
    this._digest = null;
  }
  update(data, inputEncoding) {
    if (this._done) throw new Error('Digest already called');
    this._chunks.push(toLatin1(data, inputEncoding));
    return this;
  }
  _compute() { return Buffer.from(native.hmacDigest(this._algo, this._key, this._chunks.join('')), 'latin1'); }
  digest(encoding) {
    if (this._done) throw new Error('Digest already called');
    this._done = true;
    const buf = this._compute();
    return encoding && encoding !== 'buffer' ? buf.toString(encoding) : buf;
  }
  write(data, enc, cb) { this.update(data, typeof enc === 'string' ? enc : undefined); if (typeof enc === 'function') enc(); else if (cb) cb(); return true; }
  end(data, enc, cb) {
    if (data !== undefined && data !== null && typeof data !== 'function') this.update(data, typeof enc === 'string' ? enc : undefined);
    if (!this._done) { this._done = true; this._digest = this._compute(); }
    this.emit('finish'); this.emit('readable'); this.emit('end');
    const done = typeof data === 'function' ? data : typeof enc === 'function' ? enc : cb;
    if (typeof done === 'function') done();
    return this;
  }
  read() { if (!this._done) { this._done = true; this._digest = this._compute(); } const d = this._digest; this._digest = null; return d; }
  setEncoding() { return this; }
}

function createHash(algorithm) { return new Hash(algorithm); }
function createHmac(algorithm, key) { return new Hmac(algorithm, key); }

function hash(algorithm, data, outputEncoding) {
  const buf = Buffer.from(native.hashDigest(String(algorithm), toLatin1(data)), 'latin1');
  const enc = outputEncoding === undefined ? 'hex' : outputEncoding;
  return enc === 'buffer' ? buf : buf.toString(enc);
}

function randomBytes(size, cb) {
  const n = Number(size);
  if (!Number.isInteger(n) || n < 0) {
    const err = new TypeError('The "size" argument must be a non-negative integer.');
    err.code = 'ERR_INVALID_ARG_TYPE';
    if (cb) { setTimeout(() => cb(err), 0); return undefined; }
    throw err;
  }
  const buf = Buffer.from(native.randomBytes(n), 'latin1');
  if (typeof cb === 'function') { setTimeout(() => cb(null, buf), 0); return undefined; }
  return buf;
}

function randomFillSync(buffer, offset, size) {
  offset = offset || 0;
  const len = size === undefined ? buffer.length - offset : size;
  const rnd = Buffer.from(native.randomBytes(len), 'latin1');
  for (let i = 0; i < len; i++) buffer[offset + i] = rnd[i];
  return buffer;
}

function randomFill(buffer, offset, size, cb) {
  if (typeof offset === 'function') { cb = offset; offset = 0; size = buffer.length; }
  else if (typeof size === 'function') { cb = size; size = buffer.length - offset; }
  setTimeout(() => {
    try { cb(null, randomFillSync(buffer, offset, size)); } catch (e) { cb(e); }
  }, 0);
}

function getRandomValues(typedArray) {
  const view = new Uint8Array(typedArray.buffer, typedArray.byteOffset, typedArray.byteLength);
  const rnd = Buffer.from(native.randomBytes(view.length), 'latin1');
  for (let i = 0; i < view.length; i++) view[i] = rnd[i];
  return typedArray;
}

function randomInt(min, max, cb) {
  if (typeof max === 'function') { cb = max; max = min; min = 0; }
  if (max === undefined) { max = min; min = 0; }
  const range = max - min;
  if (range <= 0 || !Number.isSafeInteger(range)) {
    const err = new RangeError('The value of "max" is out of range.');
    err.code = 'ERR_OUT_OF_RANGE';
    if (cb) { setTimeout(() => cb(err), 0); return undefined; }
    throw err;
  }
  const compute = () => {
    const bytes = Math.ceil(Math.log2(range) / 8) || 1;
    let value;
    do {
      const rnd = Buffer.from(native.randomBytes(bytes + 2), 'latin1');
      value = 0;
      for (let i = 0; i < bytes + 2; i++) value = value * 256 + rnd[i];
    } while (value >= Math.floor((256 ** (bytes + 2)) / range) * range);
    return min + (value % range);
  };
  if (typeof cb === 'function') { setTimeout(() => cb(null, compute()), 0); return undefined; }
  return compute();
}

function randomUUID() {
  const b = Buffer.from(native.randomBytes(16), 'latin1');
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = b.toString('hex');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

function getHashes() { return ['sha1', 'sha224', 'sha256', 'sha384', 'sha512', 'md5']; }
function getCiphers() { return []; }
function getFips() { return 0; }
function setFips() {}

function timingSafeEqual(a, b) {
  const ba = Buffer.isBuffer(a) ? a : Buffer.from(a);
  const bb = Buffer.isBuffer(b) ? b : Buffer.from(b);
  if (ba.length !== bb.length) {
    const err = new RangeError('Input buffers must have the same byte length');
    err.code = 'ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH';
    throw err;
  }
  let diff = 0;
  for (let i = 0; i < ba.length; i++) diff |= ba[i] ^ bb[i];
  return diff === 0;
}

const constants = {
  OPENSSL_VERSION_NUMBER: 0,
  RSA_PKCS1_PADDING: 1,
  RSA_NO_PADDING: 3,
  RSA_PKCS1_OAEP_PADDING: 4,
  RSA_PKCS1_PSS_PADDING: 6,
};

const webcrypto = { getRandomValues, randomUUID, subtle: {} };

module.exports = {
  createHash, createHmac, hash,
  randomBytes, randomFillSync, randomFill, randomInt, randomUUID,
  getRandomValues, getHashes, getCiphers, getFips, setFips, timingSafeEqual,
  constants, webcrypto, Hash, Hmac,
};
