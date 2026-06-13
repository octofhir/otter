'use strict';
// `node:string_decoder` — decode Buffers to strings across chunk boundaries,
// keeping incomplete multi-byte sequences buffered until completed.

const { Buffer } = require('buffer');

function normalizeEncoding(enc) {
  const e = String(enc || 'utf8').toLowerCase();
  switch (e) {
    case 'utf8': case 'utf-8': return 'utf8';
    case 'ucs2': case 'ucs-2': case 'utf16le': case 'utf-16le': return 'utf16le';
    case 'latin1': case 'binary': return 'latin1';
    case 'base64': return 'base64';
    case 'hex': return 'hex';
    case 'ascii': return 'ascii';
    default: throw new TypeError(`Unknown encoding: ${enc}`);
  }
}

// How many bytes a leading byte announces for a UTF-8 sequence (0 if continuation/invalid).
function utf8SeqLen(byte) {
  if (byte <= 0x7f) return 1;
  if ((byte & 0xe0) === 0xc0) return 2;
  if ((byte & 0xf0) === 0xe0) return 3;
  if ((byte & 0xf8) === 0xf0) return 4;
  return 0;
}

class StringDecoder {
  constructor(encoding) {
    this.encoding = normalizeEncoding(encoding);
    this._pending = Buffer.alloc(0);
  }

  write(buffer) {
    if (typeof buffer === 'string') return buffer;
    if (buffer.length === 0 && this._pending.length === 0) return '';
    const data = this._pending.length ? Buffer.concat([this._pending, buffer]) : Buffer.from(buffer);

    if (this.encoding === 'utf8') {
      let completeEnd = data.length;
      // Walk back over up to 3 trailing continuation bytes to find an incomplete tail.
      for (let i = 1; i <= 3 && i <= data.length; i++) {
        const pos = data.length - i;
        const need = utf8SeqLen(data[pos]);
        if (need === 0) continue; // continuation byte; keep scanning back
        if (need > i) { completeEnd = pos; } // sequence starts here but is incomplete
        break;
      }
      this._pending = data.slice(completeEnd);
      return data.slice(0, completeEnd).toString('utf8');
    }

    if (this.encoding === 'utf16le') {
      const completeEnd = data.length - (data.length % 2);
      this._pending = data.slice(completeEnd);
      return data.slice(0, completeEnd).toString('utf16le');
    }

    if (this.encoding === 'base64') {
      const completeEnd = data.length - (data.length % 3);
      this._pending = data.slice(completeEnd);
      return data.slice(0, completeEnd).toString('base64');
    }

    this._pending = Buffer.alloc(0);
    return data.toString(this.encoding);
  }

  end(buffer) {
    let out = '';
    if (buffer && buffer.length) out = this.write(buffer);
    if (this._pending.length) {
      out += this._pending.toString(this.encoding);
      this._pending = Buffer.alloc(0);
    }
    return out;
  }
}

module.exports = { StringDecoder };
