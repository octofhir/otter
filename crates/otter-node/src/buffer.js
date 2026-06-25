'use strict';
// `node:buffer` — Buffer implemented as a Uint8Array subclass in JS.
//
// Buffer is also a global in Node, and `instanceof` must be consistent between
// the global and `require('buffer').Buffer`. So the class is defined once and
// cached on `globalThis.Buffer`; subsequent shim runs reuse it.

const kMaxLength = 0x7fffffff;
let inspectMaxBytes = 50;

let Buffer = (typeof globalThis !== 'undefined' && globalThis.Buffer) || null;

if (!Buffer) {
  // ---- encoding helpers ----
  const enc = {
    normalize(encoding) {
      if (encoding === undefined) return 'utf8';
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
      : actual === undefined ? 'undefined'
      : Array.isArray(actual) ? 'an instance of Array'
      : actual && typeof actual === 'object' ? `an instance of ${actual.constructor && actual.constructor.name || 'Object'}`
      : `type ${typeof actual} (${String(actual)})`;
    return codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
      `The "${name}" argument must be of type ${expected}. Received ${received}`);
  }

  function addNumericSeparator(str) {
    let out = '';
    let i = str.length;
    const start = str[0] === '-' ? 1 : 0;
    for (; i >= start + 4; i -= 3) out = `_${str.slice(i - 3, i)}${out}`;
    return `${str.slice(0, i)}${out}`;
  }

  function formatReceived(value) {
    if (typeof value === 'bigint') {
      let s = String(value);
      if (value > 2n ** 32n || value < -(2n ** 32n)) s = addNumericSeparator(s);
      return `${s}n`;
    }
    if (typeof value === 'number') {
      if (value === 0 && 1 / value < 0) return '-0';
      if (!Number.isInteger(value)) return String(value);
      let s = String(value);
      if (value > 2 ** 32 || value < -(2 ** 32)) s = addNumericSeparator(s);
      return s;
    }
    return String(value);
  }

  function outOfRange(name, range, value) {
    return codedError(RangeError, 'ERR_OUT_OF_RANGE',
      `The value of "${name}" is out of range. It must be ${range}. Received ${formatReceived(value)}`);
  }

  function checkedBufferSize(size) {
    if (typeof size !== 'number') throw invalidArgType('size', 'number', size);
    if (!Number.isFinite(size) || size < 0 || size > kMaxLength) {
      throw outOfRange('size', `>= 0 and <= ${kMaxLength}`, size);
    }
    return Math.trunc(size);
  }

  function checkedInspectMaxBytes(value) {
    if (typeof value !== 'number') throw invalidArgType('INSPECT_MAX_BYTES', 'number', value);
    // Node validates `>= 0` only — non-integers and Infinity are accepted.
    if (Number.isNaN(value) || value < 0) {
      throw outOfRange('INSPECT_MAX_BYTES', '>= 0', value);
    }
    return value;
  }

  function unknownEncoding(encoding) {
    return codedError(TypeError, 'ERR_UNKNOWN_ENCODING', `Unknown encoding: ${encoding}`);
  }

  function invalidArgValue(name, value) {
    return codedError(TypeError, 'ERR_INVALID_ARG_VALUE',
      `The argument '${name}' is invalid. Received ${value}`);
  }

  function invalidBufferTarget(name, value) {
    const received = value === undefined ? 'undefined'
      : value === null ? 'null'
      : typeof value === 'string' ? `type string ('${value}')`
      : value && typeof value === 'object' ? `an instance of ${value.constructor && value.constructor.name || 'Object'}`
      : `type ${typeof value} (${String(value)})`;
    return codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
      `The "${name}" argument must be an instance of Buffer or Uint8Array. Received ${received}`);
  }

  function invalidState(message) {
    return codedError(Error, 'ERR_INVALID_STATE', message);
  }

  function bufferOutOfBounds(name) {
    if (name === undefined) {
      return codedError(RangeError, 'ERR_BUFFER_OUT_OF_BOUNDS',
        'Attempt to access memory outside buffer bounds');
    }
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
        } else {
          code = 0xfffd;
        }
      } else if (code >= 0xd800 && code <= 0xdfff) {
        code = 0xfffd;
      }
      if (code < 0x80) out.push(code);
      else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
      else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
      else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 0x3f), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
    }
    return out;
  }

  function utf8WriteLength(bytes, max) {
    let n = 0;
    while (n < bytes.length && n < max) {
      const b = bytes[n];
      let size;
      if (b < 0x80) size = 1;
      else if ((b & 0xe0) === 0xc0) size = 2;
      else if ((b & 0xf0) === 0xe0) size = 3;
      else size = 4;
      if (n + size > max) break;
      n += size;
    }
    return n;
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
  function hexNibble(code) {
    if (code >= 48 && code <= 57) return code - 48; // 0-9
    if (code >= 97 && code <= 102) return code - 87; // a-f
    if (code >= 65 && code <= 70) return code - 55; // A-F
    return -1;
  }
  function hexToBytes(str) {
    const clean = String(str);
    const out = [];
    for (let i = 0; i + 1 < clean.length; i += 2) {
      const hi = hexNibble(clean.charCodeAt(i));
      const lo = hexNibble(clean.charCodeAt(i + 1));
      // Node's hex decoder stops at the first byte it cannot fully decode.
      if (hi === -1 || lo === -1) break;
      out.push((hi << 4) | lo);
    }
    return out;
  }

  function base64ToBytes(str, url) {
    const input = String(str);
    let s = '';
    for (let i = 0; i < input.length; i++) {
      const ch = input[i];
      if (ch === '=') break;
      if ((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z') || (ch >= '0' && ch <= '9') || ch === '+' || ch === '/') s += ch;
      else if (ch === '-') s += '+';
      else if (ch === '_') s += '/';
    }
    const bin = (typeof atob === 'function') ? atob(s) : '';
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

  function base64ByteLength(str) {
    let len = 0;
    for (let i = 0; i < str.length; i++) {
      const ch = str[i];
      if (ch === '=') break;
      if ((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z') ||
          (ch >= '0' && ch <= '9') || ch === '+' || ch === '/' ||
          ch === '-' || ch === '_') {
        len++;
      }
    }
    return Math.floor((len * 3) / 4);
  }

  function bytesFromArrayBufferInput(input) {
    try {
      if (input instanceof ArrayBuffer) return new Uint8Array(input);
      if (ArrayBuffer.isView(input)) {
        return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
      }
    } catch {
      throw invalidState('Cannot validate a detached ArrayBuffer');
    }
    throw invalidArgType('input', 'ArrayBuffer, Buffer, TypedArray, or DataView', input);
  }

  function isAsciiBytes(input) {
    const bytes = bytesFromArrayBufferInput(input);
    for (let i = 0; i < bytes.length; i++) if (bytes[i] > 0x7f) return false;
    return true;
  }

  function validTrail(byte) {
    return (byte & 0xc0) === 0x80;
  }

  function isUtf8Bytes(input) {
    const bytes = bytesFromArrayBufferInput(input);
    for (let i = 0; i < bytes.length;) {
      const b0 = bytes[i++];
      if (b0 <= 0x7f) continue;
      if (b0 >= 0xc2 && b0 <= 0xdf) {
        if (i >= bytes.length || !validTrail(bytes[i++])) return false;
        continue;
      }
      if (b0 === 0xe0) {
        if (i + 1 >= bytes.length || bytes[i] < 0xa0 || bytes[i] > 0xbf || !validTrail(bytes[i + 1])) return false;
        i += 2;
        continue;
      }
      if ((b0 >= 0xe1 && b0 <= 0xec) || (b0 >= 0xee && b0 <= 0xef)) {
        if (i + 1 >= bytes.length || !validTrail(bytes[i]) || !validTrail(bytes[i + 1])) return false;
        i += 2;
        continue;
      }
      if (b0 === 0xed) {
        if (i + 1 >= bytes.length || bytes[i] < 0x80 || bytes[i] > 0x9f || !validTrail(bytes[i + 1])) return false;
        i += 2;
        continue;
      }
      if (b0 === 0xf0) {
        if (i + 2 >= bytes.length || bytes[i] < 0x90 || bytes[i] > 0xbf || !validTrail(bytes[i + 1]) || !validTrail(bytes[i + 2])) return false;
        i += 3;
        continue;
      }
      if (b0 >= 0xf1 && b0 <= 0xf3) {
        if (i + 2 >= bytes.length || !validTrail(bytes[i]) || !validTrail(bytes[i + 1]) || !validTrail(bytes[i + 2])) return false;
        i += 3;
        continue;
      }
      if (b0 === 0xf4) {
        if (i + 2 >= bytes.length || bytes[i] < 0x80 || bytes[i] > 0x8f || !validTrail(bytes[i + 1]) || !validTrail(bytes[i + 2])) return false;
        i += 3;
        continue;
      }
      return false;
    }
    return true;
  }

  function isArrayIndexKey(key) {
    if (!/^(?:0|[1-9][0-9]*)$/.test(key)) return false;
    return Number(key) < 0xffffffff;
  }
  function inspectExtraValue(value, ctx) {
    if (ctx && typeof ctx.inspect === 'function') return ctx.inspect(value);
    return String(value);
  }
  function inspectBuffer(buf, recurseTimes, ctx) {
    const max = Buffer.INSPECT_MAX_BYTES;
    const len = Math.min(buf.length, max);
    const parts = new Array(len);
    for (let i = 0; i < len; i++) parts[i] = hexChars[buf[i] >> 4] + hexChars[buf[i] & 0xf];
    let body = parts.join(' ');
    const remaining = buf.length - max;
    if (remaining > 0) body += `${body ? ' ' : ''}... ${remaining} more byte${remaining > 1 ? 's' : ''}`;
    // Own enumerable non-index properties are appended, e.g.
    // `<Buffer 31 32, prop: 123>`, mirroring Node's buffer inspector.
    const extras = [];
    for (const key of Object.keys(buf)) {
      if (isArrayIndexKey(key)) continue;
      extras.push(`${key}: ${inspectExtraValue(buf[key], ctx)}`);
    }
    for (const sym of Object.getOwnPropertySymbols(buf)) {
      const desc = Object.getOwnPropertyDescriptor(buf, sym);
      if (desc && desc.enumerable) extras.push(`[${sym.toString()}]: ${inspectExtraValue(buf[sym], ctx)}`);
    }
    if (extras.length) body += `${body ? ', ' : ''}${extras.join(', ')}`;
    // The receiver's constructor name is used (so a generic call on a plain
    // Uint8Array reads `<Uint8Array …>`), and the space after it is always
    // emitted, making an empty buffer `<Buffer >`.
    const name = (buf.constructor && buf.constructor.name) || 'Buffer';
    return `<${name} ${body}>`;
  }

  function bytesFromString(str, encoding) {
    const e = enc.normalize(encoding) || 'utf8';
    switch (e) {
      case 'utf8': return utf8ToBytes(str);
      case 'ascii': { const o = []; for (let i = 0; i < str.length; i++) o.push(str.charCodeAt(i) & 0xff); return o; }
      case 'latin1': { const o = []; for (let i = 0; i < str.length; i++) o.push(str.charCodeAt(i) & 0xff); return o; }
      case 'utf16le': { const o = []; for (let i = 0; i < str.length; i++) { const c = str.charCodeAt(i); o.push(c & 0xff, c >> 8); } return o; }
      case 'hex': return hexToBytes(str);
      case 'base64': return base64ToBytes(str, false);
      case 'base64url': return base64ToBytes(str, true);
      default: return utf8ToBytes(str);
    }
  }

  const BufferImpl = class extends Uint8Array {
    constructor(arg, byteOffset, length) {
      if (typeof arg === 'number') {
        super(checkedBufferSize(arg));
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
      // Node coerces start/end to integers: NaN and negative start clamp to 0,
      // a start past the end yields '', and end is clamped to the length.
      let s = start === undefined ? 0 : Number(start);
      if (Number.isNaN(s)) s = 0;
      s = Math.trunc(s);
      if (s < 0) s = 0;
      if (s > len) return '';
      let e2 = end === undefined ? len : Number(end);
      if (Number.isNaN(e2)) e2 = 0;
      e2 = Math.trunc(e2);
      if (e2 > len) e2 = len;
      if (e2 <= s) return '';
      start = s;
      end = e2;
      const e = enc.normalize(encoding);
      if (e === undefined) throw unknownEncoding(encoding);
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
      if (!(other instanceof Uint8Array)) throw argTypeError('otherBuffer', 'an instance of Buffer or Uint8Array', other);
      if (this === other) return true;
      if (this.length !== other.length) return false;
      for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
      return true;
    }

    compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
      if (!(target instanceof Uint8Array)) throw invalidBufferTarget('target', target);
      function offsetArg(name, value, def) {
        if (value === undefined) return def;
        if (typeof value !== 'number') throw invalidArgType(name, 'number', value);
        return value;
      }
      targetStart = offsetArg('targetStart', targetStart, 0);
      targetEnd = offsetArg('targetEnd', targetEnd, target.length);
      sourceStart = offsetArg('sourceStart', sourceStart, 0);
      sourceEnd = offsetArg('sourceEnd', sourceEnd, this.length);
      function checkOffset(name, value, max) {
        if (!Number.isFinite(value) || value < 0 || value > max) {
          throw outOfRange(name, `>= 0 && <= ${max}`, value);
        }
        return Math.trunc(value);
      }
      if (!Number.isFinite(targetStart) || targetStart < 0) {
        throw outOfRange('targetStart', `>= 0 && <= ${target.length}`, targetStart);
      }
      targetStart = targetStart > target.length ? Math.trunc(targetStart) : checkOffset('targetStart', targetStart, target.length);
      targetEnd = checkOffset('targetEnd', targetEnd, target.length);
      sourceStart = checkOffset('sourceStart', sourceStart, this.length);
      sourceEnd = checkOffset('sourceEnd', sourceEnd, this.length);
      if (targetEnd <= targetStart) return sourceEnd <= sourceStart ? 0 : 1;
      if (sourceEnd <= sourceStart) return -1;
      return Buffer.compare(this.subarray(sourceStart, sourceEnd), target.subarray(targetStart, targetEnd));
    }

    inspect(recurseTimes, ctx) { return inspectBuffer(this, recurseTimes, ctx); }

    write(string, offset, length, encoding) {
      if (offset === undefined) { offset = 0; length = this.length; encoding = 'utf8'; }
      else if (typeof offset === 'string') {
        if (length !== undefined) throw invalidArgType('offset', 'number', offset);
        encoding = offset; offset = 0; length = this.length;
      }
      else if (typeof length === 'string') { encoding = length; length = this.length - offset; }
      if (typeof offset !== 'number') throw invalidArgType('offset', 'number', offset);
      if (!Number.isFinite(offset) || offset < 0 || offset > this.length) {
        throw outOfRange('offset', `>= 0 && <= ${this.length}`, offset);
      }
      offset = offset | 0;
      const e = enc.normalize(encoding);
      if (e === undefined) throw unknownEncoding(encoding);
      const bytes = bytesFromString(string, e);
      const max = length === undefined ? this.length - offset : Math.min(length | 0, this.length - offset);
      let n = Math.min(bytes.length, max);
      if (e === 'utf8') n = utf8WriteLength(bytes, n);
      else if (e === 'utf16le' && (n & 1)) n--;
      for (let i = 0; i < n; i++) this[offset + i] = bytes[i];
      return n;
    }

    // Internal single-encoding writers. Unlike `write`, an out-of-bounds
    // offset/length is a hard ERR_BUFFER_OUT_OF_BOUNDS rather than a clamp.
    asciiWrite(string, offset, length) { return fixedWrite(this, string, offset, length, 'latin1'); }
    latin1Write(string, offset, length) { return fixedWrite(this, string, offset, length, 'latin1'); }
    utf8Write(string, offset, length) { return fixedWrite(this, string, offset, length, 'utf8'); }
    base64Write(string, offset, length) { return fixedWrite(this, string, offset, length, 'base64'); }
    base64urlWrite(string, offset, length) { return fixedWrite(this, string, offset, length, 'base64url'); }
    hexWrite(string, offset, length) { return fixedWrite(this, string, offset, length, 'hex'); }
    ucs2Write(string, offset, length) { return fixedWrite(this, string, offset, length, 'utf16le'); }

    // Internal single-encoding slicers, mirroring Node's lib/buffer.js surface.
    asciiSlice(start, end) { return this.toString('ascii', start, end); }
    latin1Slice(start, end) { return this.toString('latin1', start, end); }
    utf8Slice(start, end) { return this.toString('utf8', start, end); }
    base64Slice(start, end) { return this.toString('base64', start, end); }
    base64urlSlice(start, end) { return this.toString('base64url', start, end); }
    hexSlice(start, end) { return this.toString('hex', start, end); }
    ucs2Slice(start, end) { return this.toString('utf16le', start, end); }

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
      } else if (value instanceof Uint8Array) {
        if (value.length === 0) throw invalidArgValue('value', value);
        for (let i = offset, j = 0; i < end; i++, j = (j + 1) % value.length) this[i] = value[j];
      } else if (value === null || value === undefined) {
        for (let i = offset; i < end; i++) this[i] = 0;
      }
      return this;
    }

    copy(target, targetStart, sourceStart, sourceEnd) {
      if (!ArrayBuffer.isView(target)) throw invalidBufferTarget('target', target);
      if (!(this instanceof Uint8Array)) throw invalidBufferTarget('source', this);
      const coerce = (v, def) => {
        if (v === undefined) return def;
        const n = Math.trunc(Number(v));
        return Number.isNaN(n) ? 0 : n;
      };
      targetStart = coerce(targetStart, 0);
      sourceStart = coerce(sourceStart, 0);
      sourceEnd = coerce(sourceEnd, this.length);
      // Negative bounds report a bare `>= 0` range, matching Node's boundsError.
      if (targetStart < 0) throw outOfRange('targetStart', '>= 0', targetStart);
      if (sourceStart < 0) throw outOfRange('sourceStart', '>= 0', sourceStart);
      if (sourceEnd < 0) throw outOfRange('sourceEnd', '>= 0', sourceEnd);
      if (sourceStart > this.length) {
        throw outOfRange('sourceStart', `>= 0 && <= ${this.length}`, sourceStart);
      }
      if (sourceEnd > this.length) sourceEnd = this.length;
      // The target is addressed byte-wise so non-`Uint8Array` views (e.g. a
      // `Uint16Array`) receive a packed byte copy.
      const targetBytes = target instanceof Uint8Array
        ? target
        : new Uint8Array(target.buffer, target.byteOffset, target.byteLength);
      if (targetStart >= targetBytes.length || sourceEnd <= sourceStart) return 0;
      const n = Math.min(sourceEnd - sourceStart, targetBytes.length - targetStart);
      for (let i = 0; i < n; i++) targetBytes[targetStart + i] = this[sourceStart + i];
      return n;
    }

    slice(start, end) { return this.subarray(start, end); }

    subarray(start, end) {
      const sub = Uint8Array.prototype.subarray.call(this, start, end);
      Object.setPrototypeOf(sub, Buffer.prototype);
      return sub;
    }

    indexOf(value, byteOffset, encoding) {
      return bidirectionalIndexOf(this, value, byteOffset, encoding, true);
    }

    lastIndexOf(value, byteOffset, encoding) {
      return bidirectionalIndexOf(this, value, byteOffset, encoding, false);
    }

    includes(value, byteOffset, encoding) { return bidirectionalIndexOf(this, value, byteOffset, encoding, true) !== -1; }

    swap16() {
      const len = this.length;
      if (len % 2 !== 0) throw invalidBufferSize('16-bits');
      for (let i = 0; i < len; i += 2) { const t = this[i]; this[i] = this[i + 1]; this[i + 1] = t; }
      return this;
    }
    swap32() {
      const len = this.length;
      if (len % 4 !== 0) throw invalidBufferSize('32-bits');
      for (let i = 0; i < len; i += 4) { let t = this[i]; this[i] = this[i + 3]; this[i + 3] = t; t = this[i + 1]; this[i + 1] = this[i + 2]; this[i + 2] = t; }
      return this;
    }
    swap64() {
      const len = this.length;
      if (len % 8 !== 0) throw invalidBufferSize('64-bits');
      for (let i = 0; i < len; i += 8) {
        for (let j = 0; j < 4; j++) {
          const t = this[i + j];
          this[i + j] = this[i + 7 - j];
          this[i + 7 - j] = t;
        }
      }
      return this;
    }
  };

  function invalidBufferSize(unit) {
    return codedError(RangeError, 'ERR_INVALID_BUFFER_SIZE', `Buffer size must be a multiple of ${unit}`);
  }

  function fixedWrite(buf, string, offset, length, encoding) {
    if (typeof string !== 'string') throw invalidArgType('string', 'string', string);
    offset = offset === undefined ? 0 : offset | 0;
    const remaining = buf.length - offset;
    length = length === undefined ? remaining : length | 0;
    if (offset < 0 || offset > buf.length || length < 0 || length > remaining) {
      throw bufferOutOfBounds();
    }
    const bytes = bytesFromString(string, encoding);
    let n = Math.min(bytes.length, length);
    if (encoding === 'utf8') n = utf8WriteLength(bytes, n);
    for (let i = 0; i < n; i++) buf[offset + i] = bytes[i];
    return n;
  }

  // ` Received ...` suffix matching Node's `invalidArgTypeHelper`.
  function argTypeReceived(val) {
    if (val === null) return ' Received null';
    if (val === undefined) return ' Received undefined';
    if (typeof val === 'function') {
      return ` Received function ${val.name}`;
    }
    if (typeof val === 'object') {
      const name = val.constructor && val.constructor.name;
      return name ? ` Received an instance of ${name}` : ' Received [Object: null prototype]';
    }
    if (typeof val === 'string') {
      const shown = val.length > 25 ? `${val.slice(0, 25)}...` : val;
      return ` Received type string ('${shown}')`;
    }
    if (typeof val === 'symbol') return ` Received type symbol (${val.toString()})`;
    return ` Received type ${typeof val} (${String(val)}${typeof val === 'bigint' ? 'n' : ''})`;
  }

  function argTypeError(name, expected, value) {
    return codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
      `The "${name}" argument must be ${expected}.${argTypeReceived(value)}`);
  }

  const nativeIndexOf = Uint8Array.prototype.indexOf;
  const nativeLastIndexOf = Uint8Array.prototype.lastIndexOf;

  // Shared forward/backward search matching Node's bidirectionalIndexOf:
  // a string second argument is the encoding; the offset is coerced to a
  // number (NaN scans the whole buffer), negatives count from the end, and an
  // empty needle resolves to the clamped offset. The first-byte scan is
  // delegated to the native typed-array search so large buffers stay fast.
  function bidirectionalIndexOf(buf, value, byteOffset, encoding, dir) {
    if (!(buf instanceof Uint8Array)) {
      throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
        `The "buffer" argument must be an instance of Buffer, TypedArray, or DataView.${argTypeReceived(buf)}`);
    }
    if (typeof byteOffset === 'string') {
      encoding = byteOffset;
      byteOffset = undefined;
    } else if (byteOffset > 0x7fffffff) {
      byteOffset = 0x7fffffff;
    } else if (byteOffset < -0x80000000) {
      byteOffset = -0x80000000;
    }
    let offset = Number(byteOffset);
    if (Number.isNaN(offset)) offset = dir ? 0 : buf.length;
    offset = Math.trunc(offset);

    let needle;
    if (typeof value === 'number') {
      const target = value & 0xff;
      if (dir) {
        let start = offset < 0 ? buf.length + offset : offset;
        if (start < 0) start = 0;
        return nativeIndexOf.call(buf, target, start);
      }
      let start = offset < 0 ? buf.length + offset : offset;
      if (start >= buf.length) start = buf.length - 1;
      if (start < 0) return -1;
      return nativeLastIndexOf.call(buf, target, start);
    }
    if (typeof value === 'string') {
      const e0 = enc.normalize(encoding);
      if (encoding !== undefined && e0 === undefined) throw unknownEncoding(encoding);
      needle = bytesFromString(value, encoding);
    } else if (value instanceof Uint8Array) {
      needle = value;
    } else {
      throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
        `The "value" argument must be one of type number or string or an instance of Buffer or Uint8Array.${argTypeReceived(value)}`);
    }

    if (needle.length === 0) {
      let empty = offset < 0 ? buf.length + offset : offset;
      if (empty < 0) empty = 0;
      if (empty > buf.length) empty = buf.length;
      return empty;
    }

    // UCS-2/UTF-16LE searches operate in 16-bit units: matches only land on
    // even byte boundaries, and a needle shorter than one code unit never
    // matches. Mirrors Node's C++ `indexOfBuffer` UCS2 path.
    const ucs2 = enc.normalize(encoding) === 'utf16le';
    if (ucs2 && (buf.length < 2 || needle.length < 2)) return -1;
    const step = ucs2 ? 2 : 1;
    const cmpLen = ucs2 ? (needle.length & ~1) : needle.length;
    const first = needle[0];
    const last = buf.length - cmpLen;

    const matchAt = (i) => {
      for (let j = 1; j < cmpLen; j++) if (buf[i + j] !== needle[j]) return false;
      return true;
    };

    if (dir) {
      let i = offset < 0 ? buf.length + offset : offset;
      if (i < 0) i = 0;
      while (i <= last) {
        const f = nativeIndexOf.call(buf, first, i);
        if (f === -1 || f > last) return -1;
        if ((!ucs2 || (f & 1) === 0) && matchAt(f)) return f;
        i = ucs2 ? f + (f & 1 ? 1 : 2) : f + 1;
      }
      return -1;
    }
    let i = offset < 0 ? buf.length + offset : offset;
    if (i > last) i = last;
    if (ucs2 && (i & 1)) i -= 1;
    while (i >= 0) {
      const f = nativeLastIndexOf.call(buf, first, i);
      if (f === -1) return -1;
      if ((!ucs2 || (f & 1) === 0) && matchAt(f)) return f;
      i = ucs2 ? f - (f & 1 ? 1 : 2) : f - 1;
    }
    return -1;
  }
  Buffer = function Buffer(arg, byteOffset, length) {
    if (typeof arg === 'number' && typeof byteOffset === 'string') {
      throw invalidArgType('string', 'string', arg);
    }
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
  Object.defineProperty(Buffer.prototype, 'toLocaleString', {
    value: Buffer.prototype.toString,
    writable: true,
    configurable: true,
    enumerable: false,
  });
  // `inspect` and the `nodejs.util.inspect.custom` symbol are the same function.
  Object.defineProperty(Buffer.prototype, Symbol.for('nodejs.util.inspect.custom'), {
    value: Buffer.prototype.inspect,
    writable: true,
    configurable: true,
    enumerable: false,
  });

  // ---- read/write integer methods via DataView ----
  const dv = (buf) => new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  function checkedOffset(buf, offset, size, allowUndefined = true) {
    if (offset === undefined) {
      if (allowUndefined) return 0;
      throw invalidArgType('offset', 'number', offset);
    }
    if (typeof offset !== 'number') throw invalidArgType('offset', 'number', offset);
    const max = Math.max(buf.length - size, 0);
    if (offset === Infinity || offset === -Infinity || offset < 0) {
      throw outOfRange('offset', `>= 0 and <= ${max}`, offset);
    }
    if (!Number.isInteger(offset)) throw outOfRange('offset', 'an integer', offset);
    if (offset + size > buf.length) {
      if (buf.length < size) throw bufferOutOfBounds();
      throw outOfRange('offset', `>= 0 and <= ${max}`, offset);
    }
    return offset;
  }
  // Validate that an integer value fits the destination width, matching Node's
  // `checkInt`. `byteLength` is the width minus one; widths above 4 bytes are
  // described with `2 ** N` ranges because the exact bound is unsafe to print.
  function checkInt(value, min, max, byteLength) {
    if (typeof value !== 'number') throw invalidArgType('value', 'number', value);
    if (value > max || value < min) {
      let range;
      if (byteLength > 3) {
        if (min === 0) {
          range = `>= 0 and < 2 ** ${(byteLength + 1) * 8}`;
        } else {
          range = `>= -(2 ** ${(byteLength + 1) * 8 - 1}) and < 2 ** ${(byteLength + 1) * 8 - 1}`;
        }
      } else {
        range = `>= ${min} and <= ${max}`;
      }
      throw outOfRange('value', range, value);
    }
  }
  function checkedByteLength(byteLength) {
    if (typeof byteLength !== 'number') throw invalidArgType('byteLength', 'number', byteLength);
    if (byteLength === Infinity || byteLength === -Infinity || byteLength < 1 || byteLength > 6) {
      throw outOfRange('byteLength', '>= 1 and <= 6', byteLength);
    }
    if (!Number.isInteger(byteLength)) throw outOfRange('byteLength', 'an integer', byteLength);
    return byteLength;
  }
  const U64 = 18446744073709551616n;
  const I64_MIN = -9223372036854775808n;
  const I64_MAX = 9223372036854775807n;
  function readBigUInt64(buf, offset, little) {
    offset = checkedOffset(buf, offset, 8);
    let value = 0n;
    if (little) {
      for (let i = 7; i >= 0; i--) value = value * 256n + BigInt(buf[offset + i]);
    } else {
      for (let i = 0; i < 8; i++) value = value * 256n + BigInt(buf[offset + i]);
    }
    return value;
  }
  function writeBigUInt64(buf, value, offset, little) {
    if (typeof value !== 'bigint') throw invalidArgType('value', 'bigint', value);
    if (value < 0n || value >= U64) throw outOfRange('value', '>= 0n and < 2n ** 64n', value);
    offset = checkedOffset(buf, offset, 8);
    let n = value;
    for (let i = 0; i < 8; i++) {
      const byte = Number(n % 256n);
      buf[offset + (little ? i : 7 - i)] = byte;
      n = n / 256n;
    }
    return offset + 8;
  }
  // Free-standing variable-width readers so the signed variants don't depend on
  // `this.readUIntLE` existing — that breaks when the methods are applied
  // generically to a plain `Uint8Array`.
  function readUIntLEImpl(buf, o, len) {
    len = checkedByteLength(len);
    o = checkedOffset(buf, o, len, false);
    let val = 0;
    let mul = 1;
    for (let i = 0; i < len; i++) { val += buf[o + i] * mul; mul *= 256; }
    return val;
  }
  function readUIntBEImpl(buf, o, len) {
    len = checkedByteLength(len);
    o = checkedOffset(buf, o, len, false);
    let val = 0;
    for (let i = 0; i < len; i++) val = val * 256 + buf[o + i];
    return val;
  }
  const proto = Buffer.prototype;
  const intMethods = {
    readUInt8(o) { o = checkedOffset(this, o, 1); return dv(this).getUint8(o); },
    readInt8(o) { o = checkedOffset(this, o, 1); return dv(this).getInt8(o); },
    readUInt16LE(o) { o = checkedOffset(this, o, 2); return dv(this).getUint16(o, true); },
    readUInt16BE(o) { o = checkedOffset(this, o, 2); return dv(this).getUint16(o, false); },
    readInt16LE(o) { o = checkedOffset(this, o, 2); return dv(this).getInt16(o, true); },
    readInt16BE(o) { o = checkedOffset(this, o, 2); return dv(this).getInt16(o, false); },
    readUInt32LE(o) { o = checkedOffset(this, o, 4); return dv(this).getUint32(o, true); },
    readUInt32BE(o) { o = checkedOffset(this, o, 4); return dv(this).getUint32(o, false); },
    readInt32LE(o) { o = checkedOffset(this, o, 4); return dv(this).getInt32(o, true); },
    readInt32BE(o) { o = checkedOffset(this, o, 4); return dv(this).getInt32(o, false); },
    readFloatLE(o) { o = checkedOffset(this, o, 4); return dv(this).getFloat32(o, true); },
    readFloatBE(o) { o = checkedOffset(this, o, 4); return dv(this).getFloat32(o, false); },
    readDoubleLE(o) { o = checkedOffset(this, o, 8); return dv(this).getFloat64(o, true); },
    readDoubleBE(o) { o = checkedOffset(this, o, 8); return dv(this).getFloat64(o, false); },
    writeUInt8(v, o) { checkInt(v, 0, 0xff, 0); o = checkedOffset(this, o, 1); dv(this).setUint8(o, v); return o + 1; },
    writeInt8(v, o) { checkInt(v, -0x80, 0x7f, 0); o = checkedOffset(this, o, 1); dv(this).setInt8(o, v); return o + 1; },
    writeUInt16LE(v, o) { checkInt(v, 0, 0xffff, 1); o = checkedOffset(this, o, 2); dv(this).setUint16(o, v, true); return o + 2; },
    writeUInt16BE(v, o) { checkInt(v, 0, 0xffff, 1); o = checkedOffset(this, o, 2); dv(this).setUint16(o, v, false); return o + 2; },
    writeInt16LE(v, o) { checkInt(v, -0x8000, 0x7fff, 1); o = checkedOffset(this, o, 2); dv(this).setInt16(o, v, true); return o + 2; },
    writeInt16BE(v, o) { checkInt(v, -0x8000, 0x7fff, 1); o = checkedOffset(this, o, 2); dv(this).setInt16(o, v, false); return o + 2; },
    writeUInt32LE(v, o) { checkInt(v, 0, 0xffffffff, 3); o = checkedOffset(this, o, 4); dv(this).setUint32(o, v, true); return o + 4; },
    writeUInt32BE(v, o) { checkInt(v, 0, 0xffffffff, 3); o = checkedOffset(this, o, 4); dv(this).setUint32(o, v, false); return o + 4; },
    writeInt32LE(v, o) { checkInt(v, -0x80000000, 0x7fffffff, 3); o = checkedOffset(this, o, 4); dv(this).setInt32(o, v, true); return o + 4; },
    writeInt32BE(v, o) { checkInt(v, -0x80000000, 0x7fffffff, 3); o = checkedOffset(this, o, 4); dv(this).setInt32(o, v, false); return o + 4; },
    writeFloatLE(v, o) { o = checkedOffset(this, o, 4); dv(this).setFloat32(o, v, true); return o + 4; },
    writeFloatBE(v, o) { o = checkedOffset(this, o, 4); dv(this).setFloat32(o, v, false); return o + 4; },
    writeDoubleLE(v, o) { o = checkedOffset(this, o, 8); dv(this).setFloat64(o, v, true); return o + 8; },
    writeDoubleBE(v, o) { o = checkedOffset(this, o, 8); dv(this).setFloat64(o, v, false); return o + 8; },
    readBigUInt64LE(o) { return readBigUInt64(this, o, true); },
    readBigUInt64BE(o) { return readBigUInt64(this, o, false); },
    readBigInt64LE(o) { const v = readBigUInt64(this, o, true); return v > I64_MAX ? v - U64 : v; },
    readBigInt64BE(o) { const v = readBigUInt64(this, o, false); return v > I64_MAX ? v - U64 : v; },
    writeBigUInt64LE(v, o) { return writeBigUInt64(this, v, o, true); },
    writeBigUInt64BE(v, o) { return writeBigUInt64(this, v, o, false); },
    writeBigInt64LE(v, o) { if (typeof v !== 'bigint') throw invalidArgType('value', 'bigint', v); if (v < I64_MIN || v > I64_MAX) throw outOfRange('value', `>= ${I64_MIN}n and <= ${I64_MAX}n`, v); return writeBigUInt64(this, v < 0n ? U64 + v : v, o, true); },
    writeBigInt64BE(v, o) { if (typeof v !== 'bigint') throw invalidArgType('value', 'bigint', v); if (v < I64_MIN || v > I64_MAX) throw outOfRange('value', `>= ${I64_MIN}n and <= ${I64_MAX}n`, v); return writeBigUInt64(this, v < 0n ? U64 + v : v, o, false); },
    readUIntLE(o, len) { return readUIntLEImpl(this, o, len); },
    readUIntBE(o, len) { return readUIntBEImpl(this, o, len); },
    readIntLE(o, len) { let val = readUIntLEImpl(this, o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    readIntBE(o, len) { let val = readUIntBEImpl(this, o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    writeUIntLE(v, o, len) { len = checkedByteLength(len); checkInt(v, 0, Math.pow(2, 8 * len) - 1, len - 1); o = checkedOffset(this, o, len, false); let val = v; for (let i = 0; i < len; i++) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeUIntBE(v, o, len) { len = checkedByteLength(len); checkInt(v, 0, Math.pow(2, 8 * len) - 1, len - 1); o = checkedOffset(this, o, len, false); let val = v; for (let i = len - 1; i >= 0; i--) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeIntLE(v, o, len) { len = checkedByteLength(len); checkInt(v, -Math.pow(2, 8 * len - 1), Math.pow(2, 8 * len - 1) - 1, len - 1); o = checkedOffset(this, o, len, false); let val = v < 0 ? v + Math.pow(2, 8 * len) : v; for (let i = 0; i < len; i++) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeIntBE(v, o, len) { len = checkedByteLength(len); checkInt(v, -Math.pow(2, 8 * len - 1), Math.pow(2, 8 * len - 1) - 1, len - 1); o = checkedOffset(this, o, len, false); let val = v < 0 ? v + Math.pow(2, 8 * len) : v; for (let i = len - 1; i >= 0; i--) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
  };
  for (const name of Object.keys(intMethods)) {
    Object.defineProperty(proto, name, { value: intMethods[name], writable: true, configurable: true, enumerable: false });
  }
  // Node exposes `Uint` spellings (`readUint8`, `writeBigUint64LE`, …) as the
  // exact same function objects as their `UInt` counterparts.
  for (const name of Object.keys(intMethods)) {
    if (!name.includes('UInt')) continue;
    const alias = name.replace('UInt', 'Uint');
    Object.defineProperty(proto, alias, { value: intMethods[name], writable: true, configurable: true, enumerable: false });
  }
  Object.defineProperty(proto, 'parent', {
    configurable: true,
    enumerable: false,
    get() { return ArrayBuffer.isView(this) ? this.buffer : undefined; },
  });
  Object.defineProperty(proto, 'offset', {
    configurable: true,
    enumerable: false,
    get() { return ArrayBuffer.isView(this) ? this.byteOffset : undefined; },
  });

  // ---- statics ----
  // Defined via defineProperty because `Buffer` inherits non-writable statics
  // (e.g. `Uint8Array.from`) through its prototype chain, and a plain
  // assignment to such an inherited property throws in strict mode.
  const statics = {
    from(value, encodingOrOffset, length) {
      if (typeof value === 'string') {
        // A non-string encoding argument is ignored (utf8); only a string that
        // names an unknown encoding throws.
        let e = 'utf8';
        if (typeof encodingOrOffset === 'string') {
          e = enc.normalize(encodingOrOffset);
          if (e === undefined) throw unknownEncoding(encodingOrOffset);
        }
        return new Buffer(bytesFromString(value, e));
      }
      if (typeof value === 'object' && value !== null) {
        if (isArrayBufferLike(value) || isSharedArrayBufferLike(value)) {
          return new Buffer(value, encodingOrOffset, length);
        }
        // A boxed/coercible value whose valueOf() differs (e.g. `new String`)
        // is reinterpreted, matching Node's fromObject ordering.
        const vo = typeof value.valueOf === 'function' ? value.valueOf() : undefined;
        if (vo != null && vo !== value && (typeof vo === 'string' || typeof vo === 'object')) {
          return statics.from(vo, encodingOrOffset, length);
        }
        if (value instanceof Uint8Array) { const b = new Buffer(value.length); b.set(value); return b; }
        // Array-like (real array, typed array, or `{length}` object) and the
        // `{buffer: <ArrayBuffer>}` shape. A non-numeric length yields empty.
        if (value.length !== undefined || isArrayBufferLike(value.buffer) || isSharedArrayBufferLike(value.buffer)) {
          if (typeof value.length !== 'number') return new Buffer(0);
          let len = Math.trunc(value.length);
          if (!Number.isFinite(len) || len < 0) len = 0;
          const b = new Buffer(len);
          for (let i = 0; i < len; i++) b[i] = Number(value[i]) & 0xff;
          return b;
        }
        if (value.type === 'Buffer' && Array.isArray(value.data)) return new Buffer(Uint8Array.from(value.data));
        // Last resort: a Symbol.toPrimitive that yields a string.
        if (typeof value[Symbol.toPrimitive] === 'function') {
          const prim = value[Symbol.toPrimitive]('string');
          if (typeof prim === 'string') return statics.from(prim, encodingOrOffset, length);
        }
      }
      throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
        `The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object.${argTypeReceived(value)}`);
    },
    alloc(size, fill, encoding) {
      size = checkedBufferSize(size);
      const b = new Buffer(size);
      if (fill !== undefined && fill !== 0) b.fill(fill, 0, b.length, encoding);
      return b;
    },
    allocUnsafe(size) {
      size = checkedBufferSize(size);
      return new Buffer(size);
    },
    allocUnsafeSlow(size) {
      size = checkedBufferSize(size);
      return new Buffer(size);
    },
    of(...args) { return new Buffer(args); },
    isBuffer(b) { return b instanceof Buffer; },
    isEncoding(e) { return typeof e === 'string' && enc.normalize(e) !== undefined; },
    byteLength(value, encoding) {
      if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) return value.byteLength;
      if (typeof value !== 'string') {
        throw invalidArgType('string', 'string or an instance of Buffer or ArrayBuffer', value);
      }
      const e = enc.normalize(encoding) || 'utf8';
      if (e === 'base64' || e === 'base64url') return base64ByteLength(value);
      if (e === 'hex') return value.length >>> 1;
      return bytesFromString(value, e).length;
    },
    concat(list, totalLength) {
      if (!Array.isArray(list)) throw argTypeError('list', 'an instance of Array', list);
      if (list.length === 0) return new Buffer(0);
      for (let i = 0; i < list.length; i++) {
        if (!(list[i] instanceof Uint8Array)) {
          throw argTypeError(`list[${i}]`, 'an instance of Buffer or Uint8Array', list[i]);
        }
      }
      // Use the real typed-array length (byteLength) so a spoofed `length`
      // getter cannot size the result or expose uninitialized memory.
      if (totalLength === undefined) {
        totalLength = 0;
        for (const b of list) totalLength += b.byteLength;
      } else {
        if (typeof totalLength !== 'number') throw invalidArgType('length', 'number', totalLength);
        if (!Number.isInteger(totalLength)) throw outOfRange('length', 'an integer', totalLength);
        if (totalLength < 0 || totalLength > kMaxLength) {
          throw outOfRange('length', `>= 0 && <= ${kMaxLength}`, totalLength);
        }
      }
      const out = new Buffer(totalLength);
      let pos = 0;
      for (const b of list) {
        if (pos >= totalLength) break;
        const blen = b.byteLength;
        const n = Math.min(blen, totalLength - pos);
        for (let i = 0; i < n; i++) out[pos + i] = b[i];
        pos += blen;
      }
      return out;
    },
    compare(a, b) {
      if (!(a instanceof Uint8Array)) throw invalidBufferTarget('buf1', a);
      if (!(b instanceof Uint8Array)) throw invalidBufferTarget('buf2', b);
      if (a === b) return 0;
      const len = Math.min(a.length, b.length);
      for (let i = 0; i < len; i++) { if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1; }
      if (a.length < b.length) return -1;
      if (a.length > b.length) return 1;
      return 0;
    },
    isView(v) { return ArrayBuffer.isView(v); },
    isAscii: isAsciiBytes,
    isUtf8: isUtf8Bytes,
    copyBytesFrom(view, offset, length) {
      if (!ArrayBuffer.isView(view) || view.BYTES_PER_ELEMENT === undefined) {
        throw invalidArgType('view', 'an instance of TypedArray', view);
      }
      if (offset === undefined) {
        offset = 0;
      } else {
        if (typeof offset !== 'number') throw invalidArgType('offset', 'number', offset);
        if (!Number.isInteger(offset) || offset < 0) throw outOfRange('offset', '>= 0', offset);
      }
      if (length !== undefined) {
        if (typeof length !== 'number') throw invalidArgType('length', 'number', length);
        if (!Number.isInteger(length) || length < 0) throw outOfRange('length', '>= 0', length);
      }
      if (view.length === 0) return new Buffer(0);
      const available = Math.max(0, view.length - offset);
      const count = length === undefined ? available : Math.min(length, available);
      if (count <= 0) return new Buffer(0);
      const u8 = new Uint8Array(view.buffer, view.byteOffset + offset * view.BYTES_PER_ELEMENT,
        count * view.BYTES_PER_ELEMENT);
      return statics.from(u8);
    },
    poolSize: 8192,
  };
  for (const key of Object.keys(statics)) {
    Object.defineProperty(Buffer, key, { value: statics[key], writable: true, configurable: true, enumerable: false });
  }
  Object.defineProperty(Buffer, 'INSPECT_MAX_BYTES', {
    get() { return inspectMaxBytes; },
    set(value) { inspectMaxBytes = checkedInspectMaxBytes(value); },
    configurable: true,
    enumerable: false,
  });

  if (typeof globalThis !== 'undefined') globalThis.Buffer = Buffer;
}

const constants = { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: 0x1fffffff };

function SlowBuffer(length) { return Buffer.allocUnsafeSlow(length); }
SlowBuffer.prototype = Buffer.prototype;

const bufferExports = {
  Buffer,
  SlowBuffer,
  constants,
  kMaxLength,
  kStringMaxLength: 0x1fffffff,
  atob: typeof atob === 'function' ? atob : undefined,
  btoa: typeof btoa === 'function' ? btoa : undefined,
  isAscii: Buffer.isAscii,
  isUtf8: Buffer.isUtf8,
};
Object.defineProperty(bufferExports, 'INSPECT_MAX_BYTES', {
  get() { return Buffer.INSPECT_MAX_BYTES; },
  set(value) { Buffer.INSPECT_MAX_BYTES = value; },
  configurable: true,
  enumerable: true,
});

module.exports = bufferExports;
