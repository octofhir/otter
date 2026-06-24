'use strict';
// `node:buffer` — Buffer implemented as a Uint8Array subclass in JS.
//
// Buffer is also a global in Node, and `instanceof` must be consistent between
// the global and `require('buffer').Buffer`. So the class is defined once and
// cached on `globalThis.Buffer`; subsequent shim runs reuse it.

const kMaxLength = 0x7fffffff;

let Buffer = (typeof globalThis !== 'undefined' && globalThis.Buffer) || null;

if (!Buffer) {
  // ---- encoding helpers ----
  const enc = {
    normalize(encoding) {
      if (!encoding) return 'utf8';
      const e = String(encoding).toLowerCase();
      switch (e) {
        case 'utf8': case 'utf-8': return 'utf8';
        case 'ucs2': case 'ucs-2': case 'utf16le': case 'utf-16le': return 'utf16le';
        case 'latin1': case 'binary': return 'latin1';
        case 'base64': return 'base64';
        case 'base64url': return 'base64url';
        case 'hex': return 'hex';
        case 'ascii': return 'ascii';
        default: return undefined;
      }
    },
  };

  function codedError(Base, code, message) {
    const err = new Base(message);
    err.code = code;
    return err;
  }

  function invalidArgType(name, expected, actual) {
    const received = actual === null ? 'null'
      : Array.isArray(actual) ? 'an instance of Array'
      : actual && typeof actual === 'object' ? `an instance of ${actual.constructor && actual.constructor.name || 'Object'}`
      : `type ${typeof actual} (${String(actual)})`;
    return codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
      `The "${name}" argument must be of type ${expected}. Received ${received}`);
  }

  function outOfRange(name, range, value) {
    return codedError(RangeError, 'ERR_OUT_OF_RANGE',
      `The value of "${name}" is out of range. It must be ${range}. Received ${value}`);
  }

  function unknownEncoding(encoding) {
    return codedError(TypeError, 'ERR_UNKNOWN_ENCODING', `Unknown encoding: ${encoding}`);
  }

  function invalidArgValue(name, value) {
    return codedError(TypeError, 'ERR_INVALID_ARG_VALUE',
      `The argument '${name}' is invalid. Received ${value}`);
  }

  function bufferOutOfBounds(name) {
    return codedError(RangeError, 'ERR_BUFFER_OUT_OF_BOUNDS',
      `"${name}" is outside of buffer bounds`);
  }

  function isArrayBufferLike(value) {
    if (!value) return false;
    try {
      return value instanceof ArrayBuffer && typeof value.byteLength === 'number';
    } catch {
      return false;
    }
  }

  function isSharedArrayBufferLike(value) {
    if (!value || !value.constructor || value.constructor.name !== 'SharedArrayBuffer') return false;
    try {
      return typeof value.byteLength === 'number';
    } catch {
      return false;
    }
  }

  function arrayBufferSliceArgs(buffer, byteOffset, length) {
    let offset = Number(byteOffset);
    if (byteOffset === undefined || Number.isNaN(offset)) offset = 0;
    offset = Math.trunc(offset);
    if (!Number.isFinite(offset) || offset < 0 || offset > buffer.byteLength) {
      throw bufferOutOfBounds('offset');
    }
    if (length === undefined) return [offset, undefined];
    let len = Number(length);
    if (Number.isNaN(len)) len = 0;
    len = Math.trunc(len);
    if (!Number.isFinite(len) || len < 0 || offset + len > buffer.byteLength) {
      throw bufferOutOfBounds('length');
    }
    return [offset, len];
  }

  function utf8ToBytes(str) {
    const out = [];
    for (let i = 0; i < str.length; i++) {
      let code = str.charCodeAt(i);
      if (code >= 0xd800 && code <= 0xdbff && i + 1 < str.length) {
        const next = str.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) {
          code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
          i++;
        }
      }
      if (code < 0x80) out.push(code);
      else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
      else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
      else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 0x3f), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
    }
    return out;
  }

  function utf8Slice(buf, start, end) {
    let res = '';
    let i = start;
    while (i < end) {
      const b0 = buf[i];
      let cp; let size;
      if (b0 < 0x80) { cp = b0; size = 1; }
      else if ((b0 & 0xe0) === 0xc0) { cp = b0 & 0x1f; size = 2; }
      else if ((b0 & 0xf0) === 0xe0) { cp = b0 & 0x0f; size = 3; }
      else if ((b0 & 0xf8) === 0xf0) { cp = b0 & 0x07; size = 4; }
      else { res += '�'; i += 1; continue; }
      if (i + size > end) { res += '�'; break; }
      for (let j = 1; j < size; j++) cp = (cp << 6) | (buf[i + j] & 0x3f);
      if (cp > 0xffff) {
        cp -= 0x10000;
        res += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
      } else {
        res += String.fromCharCode(cp);
      }
      i += size;
    }
    return res;
  }

  const hexChars = '0123456789abcdef';
  function hexSlice(buf, start, end) {
    let out = '';
    for (let i = start; i < end; i++) out += hexChars[buf[i] >> 4] + hexChars[buf[i] & 0xf];
    return out;
  }
  function hexToBytes(str) {
    const clean = String(str);
    const len = clean.length >> 1;
    const out = new Array(len);
    for (let i = 0; i < len; i++) out[i] = parseInt(clean.substr(i * 2, 2), 16);
    return out;
  }

  function base64ToBytes(str, url) {
    let s = String(str).replace(/[^A-Za-z0-9+/\-_]/g, '');
    if (url) s = s.replace(/-/g, '+').replace(/_/g, '/');
    const bin = (typeof atob === 'function') ? atob(s.replace(/=+$/, '')) : '';
    const out = new Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i) & 0xff;
    return out;
  }
  function base64Slice(buf, start, end, url) {
    let bin = '';
    for (let i = start; i < end; i++) bin += String.fromCharCode(buf[i]);
    let b = (typeof btoa === 'function') ? btoa(bin) : '';
    if (url) b = b.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    return b;
  }

  function bytesFromString(str, encoding) {
    const e = enc.normalize(encoding) || 'utf8';
    switch (e) {
      case 'utf8': return utf8ToBytes(str);
      case 'ascii': { const o = []; for (let i = 0; i < str.length; i++) o.push(str.charCodeAt(i) & 0x7f); return o; }
      case 'latin1': { const o = []; for (let i = 0; i < str.length; i++) o.push(str.charCodeAt(i) & 0xff); return o; }
      case 'utf16le': { const o = []; for (let i = 0; i < str.length; i++) { const c = str.charCodeAt(i); o.push(c & 0xff, c >> 8); } return o; }
      case 'hex': return hexToBytes(str);
      case 'base64': return base64ToBytes(str, false);
      case 'base64url': return base64ToBytes(str, true);
      default: return utf8ToBytes(str);
    }
  }

  const BufferImpl = class Buffer extends Uint8Array {
    constructor(arg, byteOffset, length) {
      if (typeof arg === 'number') {
        super(arg < 0 ? 0 : arg);
      } else if (isArrayBufferLike(arg) || isSharedArrayBufferLike(arg)) {
        const range = arrayBufferSliceArgs(arg, byteOffset, length);
        if (range[1] === undefined) super(arg, range[0]);
        else super(arg, range[0], range[1]);
      } else if (arg && (Array.isArray(arg) || ArrayBuffer.isView(arg) || typeof arg[Symbol.iterator] === 'function')) {
        super(Uint8Array.from(arg));
      } else {
        super(0);
      }
    }

    get [Symbol.toStringTag]() { return 'Uint8Array'; }

    toString(encoding, start, end) {
      const len = this.length;
      start = start === undefined ? 0 : (start | 0);
      end = end === undefined ? len : (end | 0);
      if (start < 0) start = 0;
      if (end > len) end = len;
      if (end <= start) return '';
      const e = enc.normalize(encoding) || 'utf8';
      switch (e) {
        case 'utf8': return utf8Slice(this, start, end);
        case 'ascii': { let s = ''; for (let i = start; i < end; i++) s += String.fromCharCode(this[i] & 0x7f); return s; }
        case 'latin1': { let s = ''; for (let i = start; i < end; i++) s += String.fromCharCode(this[i]); return s; }
        case 'utf16le': { let s = ''; for (let i = start; i + 1 < end; i += 2) s += String.fromCharCode(this[i] | (this[i + 1] << 8)); return s; }
        case 'hex': return hexSlice(this, start, end);
        case 'base64': return base64Slice(this, start, end, false);
        case 'base64url': return base64Slice(this, start, end, true);
        default: return utf8Slice(this, start, end);
      }
    }

    toLocaleString(...args) { return this.toString(...args); }

    toJSON() { return { type: 'Buffer', data: Array.prototype.slice.call(this) }; }

    equals(other) {
      if (!(other instanceof Uint8Array)) throw new TypeError('The "otherBuffer" argument must be an instance of Buffer or Uint8Array.');
      if (this === other) return true;
      if (this.length !== other.length) return false;
      for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
      return true;
    }

    compare(other) { return Buffer.compare(this, other); }

    write(string, offset, length, encoding) {
      if (offset === undefined) { offset = 0; length = this.length; encoding = 'utf8'; }
      else if (typeof offset === 'string') { encoding = offset; offset = 0; length = this.length; }
      else if (typeof length === 'string') { encoding = length; length = this.length - offset; }
      offset = offset | 0;
      const bytes = bytesFromString(string, encoding);
      const max = length === undefined ? this.length - offset : Math.min(length | 0, this.length - offset);
      const n = Math.min(bytes.length, max);
      for (let i = 0; i < n; i++) this[offset + i] = bytes[i];
      return n;
    }

    fill(value, offset, end, encoding) {
      if (this.length > this.byteLength) {
        throw codedError(RangeError, 'ERR_BUFFER_OUT_OF_BOUNDS',
          'Attempt to access memory outside buffer bounds');
      }
      if (typeof offset === 'string') {
        encoding = offset;
        offset = 0;
        end = this.length;
      } else if (typeof end === 'string') {
        encoding = end;
        end = this.length;
      }
      offset = offset === undefined ? 0 : offset;
      end = end === undefined ? this.length : end;
      if (typeof offset !== 'number') throw invalidArgType('offset', 'number', offset);
      if (typeof end !== 'number') throw invalidArgType('end', 'number', end);
      if (!Number.isFinite(offset) || offset < 0 || offset > this.length) {
        throw outOfRange('offset', `>= 0 && <= ${this.length}`, offset);
      }
      if (!Number.isFinite(end) || end < 0 || end > this.length) {
        throw outOfRange('end', `>= 0 && <= ${this.length}`, end);
      }
      offset = offset >>> 0;
      end = end >>> 0;
      if (end <= offset) {
        if (typeof value === 'string' && encoding !== undefined) {
          if (typeof encoding !== 'string') throw invalidArgType('encoding', 'string', encoding);
          const e0 = enc.normalize(encoding);
          if (e0 === undefined) throw unknownEncoding(encoding);
        }
        return this;
      }
      if (typeof value === 'string') {
        if (encoding !== undefined && typeof encoding !== 'string') {
          throw invalidArgType('encoding', 'string', encoding);
        }
        const e = enc.normalize(encoding);
        if (e === undefined) throw unknownEncoding(encoding);
        if (e === 'hex' && value.length > 0 && !/^(?:[0-9a-fA-F]{2})+$/.test(value)) {
          throw invalidArgValue('value', value);
        }
        const bytes = bytesFromString(value, e);
        if (bytes.length === 0) return this;
        for (let i = offset, j = 0; i < end; i++, j = (j + 1) % bytes.length) this[i] = bytes[j];
      } else if (typeof value === 'number') {
        for (let i = offset; i < end; i++) this[i] = value & 0xff;
      } else if (value instanceof Uint8Array && value.length) {
        for (let i = offset, j = 0; i < end; i++, j = (j + 1) % value.length) this[i] = value[j];
      } else if (value === null || value === undefined) {
        for (let i = offset; i < end; i++) this[i] = 0;
      }
      return this;
    }

    copy(target, targetStart, sourceStart, sourceEnd) {
      targetStart = targetStart || 0;
      sourceStart = sourceStart || 0;
      sourceEnd = sourceEnd === undefined ? this.length : sourceEnd;
      let n = 0;
      for (let i = sourceStart; i < sourceEnd && targetStart + n < target.length; i++) {
        target[targetStart + n] = this[i];
        n++;
      }
      return n;
    }

    slice(start, end) { return this.subarray(start, end); }

    subarray(start, end) {
      const sub = Uint8Array.prototype.subarray.call(this, start, end);
      Object.setPrototypeOf(sub, Buffer.prototype);
      return sub;
    }

    indexOf(value, byteOffset, encoding) {
      const hay = this;
      let needle;
      if (typeof value === 'number') {
        for (let i = (byteOffset | 0); i < hay.length; i++) if (hay[i] === (value & 0xff)) return i;
        return -1;
      }
      if (typeof value === 'string') needle = bytesFromString(value, encoding);
      else needle = value;
      if (needle.length === 0) return 0;
      const start = byteOffset | 0;
      for (let i = start; i <= hay.length - needle.length; i++) {
        let found = true;
        for (let j = 0; j < needle.length; j++) if (hay[i + j] !== needle[j]) { found = false; break; }
        if (found) return i;
      }
      return -1;
    }

    includes(value, byteOffset, encoding) { return this.indexOf(value, byteOffset, encoding) !== -1; }

    swap16() { for (let i = 0; i < this.length; i += 2) { const t = this[i]; this[i] = this[i + 1]; this[i + 1] = t; } return this; }
    swap32() { for (let i = 0; i < this.length; i += 4) { let t = this[i]; this[i] = this[i + 3]; this[i + 3] = t; t = this[i + 1]; this[i + 1] = this[i + 2]; this[i + 2] = t; } return this; }
  };
  Buffer = function Buffer(arg, byteOffset, length) {
    return new BufferImpl(arg, byteOffset, length);
  };
  Buffer.prototype = BufferImpl.prototype;
  Object.setPrototypeOf(Buffer, BufferImpl);
  Object.defineProperty(Buffer.prototype, 'constructor', {
    value: Buffer,
    writable: true,
    configurable: true,
    enumerable: false,
  });

  // ---- read/write integer methods via DataView ----
  const dv = (buf) => new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const proto = Buffer.prototype;
  const intMethods = {
    readUInt8(o = 0) { return dv(this).getUint8(o); },
    readInt8(o = 0) { return dv(this).getInt8(o); },
    readUInt16LE(o = 0) { return dv(this).getUint16(o, true); },
    readUInt16BE(o = 0) { return dv(this).getUint16(o, false); },
    readInt16LE(o = 0) { return dv(this).getInt16(o, true); },
    readInt16BE(o = 0) { return dv(this).getInt16(o, false); },
    readUInt32LE(o = 0) { return dv(this).getUint32(o, true); },
    readUInt32BE(o = 0) { return dv(this).getUint32(o, false); },
    readInt32LE(o = 0) { return dv(this).getInt32(o, true); },
    readInt32BE(o = 0) { return dv(this).getInt32(o, false); },
    readFloatLE(o = 0) { return dv(this).getFloat32(o, true); },
    readFloatBE(o = 0) { return dv(this).getFloat32(o, false); },
    readDoubleLE(o = 0) { return dv(this).getFloat64(o, true); },
    readDoubleBE(o = 0) { return dv(this).getFloat64(o, false); },
    writeUInt8(v, o = 0) { dv(this).setUint8(o, v); return o + 1; },
    writeInt8(v, o = 0) { dv(this).setInt8(o, v); return o + 1; },
    writeUInt16LE(v, o = 0) { dv(this).setUint16(o, v, true); return o + 2; },
    writeUInt16BE(v, o = 0) { dv(this).setUint16(o, v, false); return o + 2; },
    writeInt16LE(v, o = 0) { dv(this).setInt16(o, v, true); return o + 2; },
    writeInt16BE(v, o = 0) { dv(this).setInt16(o, v, false); return o + 2; },
    writeUInt32LE(v, o = 0) { dv(this).setUint32(o, v, true); return o + 4; },
    writeUInt32BE(v, o = 0) { dv(this).setUint32(o, v, false); return o + 4; },
    writeInt32LE(v, o = 0) { dv(this).setInt32(o, v, true); return o + 4; },
    writeInt32BE(v, o = 0) { dv(this).setInt32(o, v, false); return o + 4; },
    writeFloatLE(v, o = 0) { dv(this).setFloat32(o, v, true); return o + 4; },
    writeFloatBE(v, o = 0) { dv(this).setFloat32(o, v, false); return o + 4; },
    writeDoubleLE(v, o = 0) { dv(this).setFloat64(o, v, true); return o + 8; },
    writeDoubleBE(v, o = 0) { dv(this).setFloat64(o, v, false); return o + 8; },
    readUIntLE(o, len) { let val = 0; let mul = 1; for (let i = 0; i < len; i++) { val += this[o + i] * mul; mul *= 256; } return val; },
    readUIntBE(o, len) { let val = 0; for (let i = 0; i < len; i++) val = val * 256 + this[o + i]; return val; },
    readIntLE(o, len) { let val = this.readUIntLE(o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    readIntBE(o, len) { let val = this.readUIntBE(o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    writeUIntLE(v, o, len) { let val = v; for (let i = 0; i < len; i++) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeUIntBE(v, o, len) { let val = v; for (let i = len - 1; i >= 0; i--) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeIntLE(v, o, len) { let val = v < 0 ? v + Math.pow(2, 8 * len) : v; return this.writeUIntLE(val, o, len); },
    writeIntBE(v, o, len) { let val = v < 0 ? v + Math.pow(2, 8 * len) : v; return this.writeUIntBE(val, o, len); },
  };
  for (const name of Object.keys(intMethods)) {
    Object.defineProperty(proto, name, { value: intMethods[name], writable: true, configurable: true, enumerable: false });
  }
  Object.defineProperty(proto, 'parent', {
    configurable: true,
    enumerable: false,
    get() { return this.buffer; },
  });

  // ---- statics ----
  // Defined via defineProperty because `Buffer` inherits non-writable statics
  // (e.g. `Uint8Array.from`) through its prototype chain, and a plain
  // assignment to such an inherited property throws in strict mode.
  const statics = {
    from(value, encodingOrOffset, length) {
      if (typeof value === 'string') return new Buffer(bytesFromString(value, encodingOrOffset));
      if (isArrayBufferLike(value) || isSharedArrayBufferLike(value)) {
        return new Buffer(value, encodingOrOffset, length);
      }
      if (value instanceof Uint8Array) { const b = new Buffer(value.length); b.set(value); return b; }
      if (Array.isArray(value) || (value && typeof value[Symbol.iterator] === 'function')) return new Buffer(Uint8Array.from(value));
      if (value && typeof value === 'object' && value.type === 'Buffer' && Array.isArray(value.data)) return new Buffer(Uint8Array.from(value.data));
      if (value && typeof value === 'object' && Object.prototype.hasOwnProperty.call(value, 'length')) {
        let len = Number(value.length);
        if (!Number.isFinite(len) || len < 0) len = 0;
        const b = new Buffer(Math.trunc(len));
        for (let i = 0; i < b.length; i++) b[i] = Number(value[i]) & 0xff;
        return b;
      }
      throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
        'The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object. ' +
        `Received an instance of ${value && value.constructor && value.constructor.name || 'Object'}`);
    },
    alloc(size, fill, encoding) {
      const b = new Buffer(size);
      if (fill !== undefined && fill !== 0) b.fill(fill, 0, b.length, encoding);
      return b;
    },
    allocUnsafe(size) { return new Buffer(size); },
    allocUnsafeSlow(size) { return new Buffer(size); },
    of(...args) { return new Buffer(args); },
    isBuffer(b) { return b instanceof Buffer; },
    isEncoding(e) { return typeof e === 'string' && enc.normalize(e) !== undefined; },
    byteLength(string, encoding) {
      if (string instanceof Uint8Array || string instanceof ArrayBuffer) return string.byteLength;
      return bytesFromString(String(string), encoding).length;
    },
    concat(list, totalLength) {
      if (totalLength === undefined) { totalLength = 0; for (const b of list) totalLength += b.length; }
      const out = new Buffer(totalLength);
      let pos = 0;
      for (const b of list) {
        if (pos >= totalLength) break;
        const n = Math.min(b.length, totalLength - pos);
        for (let i = 0; i < n; i++) out[pos + i] = b[i];
        pos += b.length;
      }
      return out;
    },
    compare(a, b) {
      if (a === b) return 0;
      const len = Math.min(a.length, b.length);
      for (let i = 0; i < len; i++) { if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1; }
      if (a.length < b.length) return -1;
      if (a.length > b.length) return 1;
      return 0;
    },
    isView(v) { return ArrayBuffer.isView(v); },
    copyBytesFrom(view, offset = 0, length) {
      const u8 = new Uint8Array(view.buffer, view.byteOffset + offset * view.BYTES_PER_ELEMENT,
        (length === undefined ? view.length - offset : length) * view.BYTES_PER_ELEMENT);
      return statics.from(u8);
    },
    poolSize: 8192,
  };
  for (const key of Object.keys(statics)) {
    Object.defineProperty(Buffer, key, { value: statics[key], writable: true, configurable: true, enumerable: false });
  }

  if (typeof globalThis !== 'undefined') globalThis.Buffer = Buffer;
}

const constants = { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: 0x1fffffff };

function SlowBuffer(length) { return Buffer.alloc(length | 0); }
SlowBuffer.prototype = Buffer.prototype;

module.exports = {
  Buffer,
  SlowBuffer,
  constants,
  kMaxLength,
  kStringMaxLength: 0x1fffffff,
  INSPECT_MAX_BYTES: 50,
  atob: typeof atob === 'function' ? atob : undefined,
  btoa: typeof btoa === 'function' ? btoa : undefined,
  isUtf8(input) { try { return Buffer.from(input).toString('utf8') !== undefined; } catch { return false; } },
};
