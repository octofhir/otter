'use strict';
// internalBinding('string_decoder') — the incomplete-character state machine
// behind the vendored string_decoder.js. State lives in a 7-byte Buffer the
// JS wrapper allocates: bytes [0,4) hold the buffered partial character,
// then missing-byte count, buffered-byte count, and the encoding id.
const { Buffer } = require('buffer');

const encodings = [
  'ascii', 'utf8', 'base64', 'ucs2', 'hex', 'binary', 'latin1',
  'utf16le', 'base64url',
];

const kIncompleteCharactersStart = 0;
const kIncompleteCharactersEnd = 4;
const kMissingBytes = 4;
const kBufferedBytes = 5;
const kEncodingField = 6;
const kSize = 7;

function toBuffer(view) {
  return Buffer.isBuffer(view)
    ? view
    : Buffer.from(view.buffer, view.byteOffset, view.byteLength);
}

// Node's native decoder classifies by bit masks (0xF6 counts as a 4-byte
// lead); the replacement-character fine print is settled later by the
// WHATWG flat decode of whatever was buffered.
function utf8Need(lead) {
  if ((lead & 0xE0) === 0xC0) return 2;
  if ((lead & 0xF0) === 0xE0) return 3;
  if ((lead & 0xF8) === 0xF0) return 4;
  return 1;
}

function decodeUtf8(state, chunk) {
  let out = '';
  let buf = chunk;
  if (state[kBufferedBytes] > 0) {
    // Feed continuation bytes into the buffered sequence; a non-continuation
    // byte cuts it short and the partial bytes decode flat (one replacement
    // per invalid position, per WHATWG).
    while (state[kMissingBytes] > 0 && buf.length > 0) {
      const byte = buf[0];
      if ((byte & 0xC0) !== 0x80) {
        state[kMissingBytes] = 0;
        break;
      }
      state[state[kBufferedBytes]] = byte;
      state[kBufferedBytes] += 1;
      state[kMissingBytes] -= 1;
      buf = buf.subarray(1);
    }
    if (state[kMissingBytes] === 0) {
      out += state.subarray(0, state[kBufferedBytes]).toString('utf8');
      state[kBufferedBytes] = 0;
    } else {
      return out;
    }
  }
  // Hold back a trailing incomplete sequence (mask-classified).
  let cut = 0;
  for (let i = 1; i <= 3 && i <= buf.length; i++) {
    const byte = buf[buf.length - i];
    if ((byte & 0xC0) === 0x80) continue;
    if (utf8Need(byte) > i) cut = i;
    break;
  }
  if (cut > 0) {
    const start = buf.length - cut;
    for (let i = 0; i < cut; i++) state[i] = buf[start + i];
    state[kBufferedBytes] = cut;
    state[kMissingBytes] = utf8Need(buf[start]) - cut;
    buf = buf.subarray(0, start);
  }
  return out + buf.toString('utf8');
}

function decodeUtf16(state, chunk) {
  let out = '';
  let buf = chunk;
  // Fill the pending window first: 2 bytes for a lone unit, 4 for a
  // held lead surrogate awaiting its pair.
  if (state[kBufferedBytes] > 0) {
    const take = Math.min(state[kMissingBytes], buf.length);
    for (let i = 0; i < take; i++) state[state[kBufferedBytes] + i] = buf[i];
    state[kBufferedBytes] += take;
    state[kMissingBytes] -= take;
    buf = buf.subarray(take);
    if (state[kMissingBytes] > 0) return '';
    out += Buffer.from(state.subarray(0, state[kBufferedBytes]))
      .toString('utf16le');
    state[kBufferedBytes] = 0;
  }
  const odd = buf.length & 1;
  let end = buf.length - odd;
  let text = buf.subarray(0, end).toString('utf16le');
  let heldLead = 0;
  if (odd === 0 && text.length > 0) {
    const last = text.charCodeAt(text.length - 1);
    if (last >= 0xD800 && last <= 0xDBFF) {
      // A trailing lead surrogate waits for its pair — but only when the
      // chunk ends on it exactly; a stray odd byte after it flushes it.
      text = text.slice(0, -1);
      heldLead = 2;
    }
  }
  out += text;
  const keep = heldLead + odd;
  if (keep > 0) {
    const start = buf.length - keep;
    for (let i = 0; i < keep; i++) state[i] = buf[start + i];
    state[kBufferedBytes] = keep;
    state[kMissingBytes] = (heldLead > 0 ? 4 : 2) - keep;
  }
  return out;
}

function decodeBase64(state, chunk, name) {
  const buffered = state[kBufferedBytes];
  let total = chunk;
  if (buffered > 0) {
    total = Buffer.concat([Buffer.from(state.subarray(0, buffered)), chunk]);
    state[kBufferedBytes] = 0;
  }
  const rem = total.length % 3;
  const end = total.length - rem;
  const out = total.subarray(0, end).toString(name);
  if (rem > 0) {
    for (let i = 0; i < rem; i++) state[i] = total[end + i];
    state[kBufferedBytes] = rem;
    state[kMissingBytes] = 3 - rem;
  } else {
    state[kMissingBytes] = 0;
  }
  return out;
}

function decode(state, view) {
  const chunk = toBuffer(view);
  switch (encodings[state[kEncodingField]]) {
    case 'utf8':
      return decodeUtf8(state, chunk);
    case 'ucs2':
    case 'utf16le':
      return decodeUtf16(state, chunk);
    case 'base64':
      return decodeBase64(state, chunk, 'base64');
    case 'base64url':
      return decodeBase64(state, chunk, 'base64url');
    case 'binary':
      return chunk.toString('latin1');
    default:
      return chunk.toString(encodings[state[kEncodingField]]);
  }
}

function flush(state) {
  const buffered = state[kBufferedBytes];
  const name = encodings[state[kEncodingField]];
  state[kBufferedBytes] = 0;
  state[kMissingBytes] = 0;
  if (buffered === 0) return '';
  if (name === 'utf8') {
    // The flat decode answers exactly how many replacements the partial
    // bytes are worth.
    return Buffer.from(state.subarray(0, buffered)).toString('utf8');
  }
  if (name === 'ucs2' || name === 'utf16le') {
    // An unpaired lead surrogate flushes as itself; a stray odd byte drops.
    const even = buffered & ~1;
    return even === 0
      ? ''
      : Buffer.from(state.subarray(0, even)).toString('utf16le');
  }
  if (name === 'base64' || name === 'base64url') {
    return Buffer.from(state.subarray(0, buffered)).toString(name);
  }
  return '';
}

module.exports = {
  encodings,
  kIncompleteCharactersStart,
  kIncompleteCharactersEnd,
  kMissingBytes,
  kBufferedBytes,
  kEncodingField,
  kSize,
  decode,
  flush,
};
