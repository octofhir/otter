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
      : actual === undefined ? 'undefined'
      : Array.isArray(actual) ? 'an instance of Array'
      : actual && typeof actual === 'object' ? `an instance of ${actual.constructor && actual.constructor.name || 'Object'}`
      : `type ${typeof actual} (${String(actual)})`;
    return codedError(TypeError, 'ERR_INVALID_ARG_TYPE',
      `The "${name}" argument must be of type ${expected}. Received ${received}`);
  }

  function formatBigInt(value) {
    let s = value < 0n ? String(-value) : String(value);
    let out = '';
    while (s.length > 3) {
      out = `_${s.slice(-3)}${out}`;
      s = s.slice(0, -3);
    }
    return `${value < 0n ? '-' : ''}${s}${out}n`;
  }

  function outOfRange(name, range, value) {
    const received = typeof value === 'bigint' ? formatBigInt(value) : String(value);
    return codedError(RangeError, 'ERR_OUT_OF_RANGE',
      `The value of "${name}" is out of range. It must be ${range}. Received ${received}`);
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
    if (!Number.isFinite(value) || value < 0 || !Number.isInteger(value)) {
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

  function invalidFirstBufferArg(value) {
    const received = value === undefined ? 'undefined' : value === null ? 'null' : String(value);
    return new TypeError('The first argument must be of type string or an instance of ' +
      `Buffer, ArrayBuffer, or Array or an Array-like Object. Received ${received}`);
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
  function hexToBytes(str) {
    const clean = String(str);
    const len = clean.length >> 1;
    const out = new Array(len);
    for (let i = 0; i < len; i++) out[i] = parseInt(clean.substr(i * 2, 2), 16);
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

  function inspectBuffer(buf) {
    const max = Buffer.INSPECT_MAX_BYTES;
    const len = Math.min(buf.length, max);
    const parts = new Array(len);
    for (let i = 0; i < len; i++) parts[i] = hexChars[buf[i] >> 4] + hexChars[buf[i] & 0xf];
    let body = parts.join(' ');
    if (buf.length > max) body += ` ... ${buf.length - max} more bytes`;
    return `<Buffer${body ? ' ' + body : ''}>`;
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
      start = start === undefined ? 0 : (start | 0);
      end = end === undefined ? len : (end | 0);
      if (start < 0) start = 0;
      if (end > len) end = len;
      if (end <= start) return '';
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
      if (!(other instanceof Uint8Array)) throw new TypeError('The "otherBuffer" argument must be an instance of Buffer or Uint8Array.');
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

    inspect() { return inspectBuffer(this); }

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
      if (!(target instanceof Uint8Array)) throw invalidBufferTarget('target', target);
      targetStart = targetStart === undefined ? 0 : Math.trunc(Number(targetStart));
      sourceStart = sourceStart === undefined ? 0 : Math.trunc(Number(sourceStart));
      sourceEnd = sourceEnd === undefined ? this.length : sourceEnd;
      sourceEnd = Math.trunc(Number(sourceEnd));
      if (!Number.isFinite(targetStart) || targetStart < 0) {
        throw outOfRange('targetStart', `>= 0 && <= ${target.length}`, targetStart);
      }
      if (!Number.isFinite(sourceStart) || sourceStart < 0) {
        throw outOfRange('sourceStart', `>= 0 && <= ${this.length}`, sourceStart);
      }
      if (!Number.isFinite(sourceEnd) || sourceEnd < 0) {
        throw outOfRange('sourceEnd', `>= 0 && <= ${this.length}`, sourceEnd);
      }
      if (sourceEnd <= sourceStart) return 0;
      if (targetStart > target.length) {
        throw outOfRange('targetStart', `>= 0 && <= ${target.length}`, targetStart);
      }
      if (sourceStart > this.length) {
        throw outOfRange('sourceStart', `>= 0 && <= ${this.length}`, sourceStart);
      }
      if (sourceEnd > this.length) sourceEnd = this.length;
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
    writeUInt8(v, o) { o = checkedOffset(this, o, 1); dv(this).setUint8(o, v); return o + 1; },
    writeInt8(v, o) { o = checkedOffset(this, o, 1); dv(this).setInt8(o, v); return o + 1; },
    writeUInt16LE(v, o) { o = checkedOffset(this, o, 2); dv(this).setUint16(o, v, true); return o + 2; },
    writeUInt16BE(v, o) { o = checkedOffset(this, o, 2); dv(this).setUint16(o, v, false); return o + 2; },
    writeInt16LE(v, o) { o = checkedOffset(this, o, 2); dv(this).setInt16(o, v, true); return o + 2; },
    writeInt16BE(v, o) { o = checkedOffset(this, o, 2); dv(this).setInt16(o, v, false); return o + 2; },
    writeUInt32LE(v, o) { o = checkedOffset(this, o, 4); dv(this).setUint32(o, v, true); return o + 4; },
    writeUInt32BE(v, o) { o = checkedOffset(this, o, 4); dv(this).setUint32(o, v, false); return o + 4; },
    writeInt32LE(v, o) { o = checkedOffset(this, o, 4); dv(this).setInt32(o, v, true); return o + 4; },
    writeInt32BE(v, o) { o = checkedOffset(this, o, 4); dv(this).setInt32(o, v, false); return o + 4; },
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
    readUIntLE(o, len) { len = checkedByteLength(len); o = checkedOffset(this, o, len, false); let val = 0; let mul = 1; for (let i = 0; i < len; i++) { val += this[o + i] * mul; mul *= 256; } return val; },
    readUIntBE(o, len) { len = checkedByteLength(len); o = checkedOffset(this, o, len, false); let val = 0; for (let i = 0; i < len; i++) val = val * 256 + this[o + i]; return val; },
    readIntLE(o, len) { let val = this.readUIntLE(o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    readIntBE(o, len) { let val = this.readUIntBE(o, len); const sub = Math.pow(2, 8 * len); if (val >= sub / 2) val -= sub; return val; },
    writeUIntLE(v, o, len) { len = checkedByteLength(len); o = checkedOffset(this, o, len, false); let val = v; for (let i = 0; i < len; i++) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeUIntBE(v, o, len) { len = checkedByteLength(len); o = checkedOffset(this, o, len, false); let val = v; for (let i = len - 1; i >= 0; i--) { this[o + i] = val & 0xff; val = Math.floor(val / 256); } return o + len; },
    writeIntLE(v, o, len) { let val = v < 0 ? v + Math.pow(2, 8 * len) : v; return this.writeUIntLE(val, o, len); },
    writeIntBE(v, o, len) { let val = v < 0 ? v + Math.pow(2, 8 * len) : v; return this.writeUIntBE(val, o, len); },
  };
  for (const name of Object.keys(intMethods)) {
    Object.defineProperty(proto, name, { value: intMethods[name], writable: true, configurable: true, enumerable: false });
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
      if (value === undefined || value === null) throw invalidFirstBufferArg(value);
      if (typeof value === 'string') {
        const e = enc.normalize(encodingOrOffset);
        if (e === undefined) throw unknownEncoding(encodingOrOffset);
        return new Buffer(bytesFromString(value, e));
      }
      if (isArrayBufferLike(value) || isSharedArrayBufferLike(value)) {
        return new Buffer(value, encodingOrOffset, length);
      }
      if (value instanceof Uint8Array) { const b = new Buffer(value.length); b.set(value); return b; }
      if (Array.isArray(value) || (value && typeof value[Symbol.iterator] === 'function')) return new Buffer(Uint8Array.from(value));
      if (value && typeof value === 'object' && (isArrayBufferLike(value.buffer) || isSharedArrayBufferLike(value.buffer))) {
        return new Buffer(value.buffer, value.byteOffset || 0, value.length);
      }
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
      return bytesFromString(value, e).length;
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
