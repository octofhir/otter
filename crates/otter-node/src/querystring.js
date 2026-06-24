'use strict';
// `node:querystring` — classic query-string parse/stringify.
// Faithful port of Node v24 lib/querystring.js + the encoder helpers from
// lib/internal/querystring.js (encodeStr / hexTable / isHexTable), inlined so
// the module has no `internal/*` dependency.

const { Buffer } = require('buffer');

// ---- internal/querystring helpers (inlined) ----

const hexTable = new Array(256);
for (let i = 0; i < 256; ++i) {
  hexTable[i] = '%' + ((i < 16 ? '0' : '') + i.toString(16)).toUpperCase();
}

const isHexTable = new Int8Array([
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0 - 15
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 16 - 31
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 32 - 47
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, // 48 - 63
  0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 64 - 79
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 80 - 95
  0, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 96 - 111
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 112 - 127
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

function ERR_INVALID_URI() {
  const e = new URIError('URI malformed');
  e.code = 'ERR_INVALID_URI';
  return e;
}

function encodeStr(str, noEscapeTable, hexTable) {
  const len = str.length;
  if (len === 0) return '';

  let out = '';
  let lastPos = 0;
  let i = 0;

  outer: for (; i < len; i++) {
    let c = str.charCodeAt(i);

    // ASCII
    while (c < 0x80) {
      if (noEscapeTable[c] !== 1) {
        if (lastPos < i) out += str.slice(lastPos, i);
        lastPos = i + 1;
        out += hexTable[c];
      }
      if (++i === len) break outer;
      c = str.charCodeAt(i);
    }

    if (lastPos < i) out += str.slice(lastPos, i);

    // Multi-byte characters ...
    if (c < 0x800) {
      lastPos = i + 1;
      out += hexTable[0xc0 | (c >> 6)] + hexTable[0x80 | (c & 0x3f)];
      continue;
    }
    if (c < 0xd800 || c >= 0xe000) {
      lastPos = i + 1;
      out += hexTable[0xe0 | (c >> 12)] +
             hexTable[0x80 | ((c >> 6) & 0x3f)] +
             hexTable[0x80 | (c & 0x3f)];
      continue;
    }
    // Surrogate pair
    ++i;
    if (i >= len) throw ERR_INVALID_URI();

    const c2 = str.charCodeAt(i) & 0x3ff;

    lastPos = i + 1;
    c = 0x10000 + (((c & 0x3ff) << 10) | c2);
    out += hexTable[0xf0 | (c >> 18)] +
           hexTable[0x80 | ((c >> 12) & 0x3f)] +
           hexTable[0x80 | ((c >> 6) & 0x3f)] +
           hexTable[0x80 | (c & 0x3f)];
  }
  if (lastPos === 0) return str;
  if (lastPos < len) return out + str.slice(lastPos);
  return out;
}

// ---- querystring ----

const unhexTable = new Int8Array([
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  +0, +1, +2, +3, +4, +5, +6, +7, +8, +9, -1, -1, -1, -1, -1, -1,
  -1, 10, 11, 12, 13, 14, 15, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, 10, 11, 12, 13, 14, 15, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
  -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
]);

function unescapeBuffer(s, decodeSpaces) {
  const out = Buffer.allocUnsafe(s.length);
  let index = 0;
  let outIndex = 0;
  let currentChar;
  let nextChar;
  let hexHigh;
  let hexLow;
  const maxLength = s.length - 2;
  let hasHex = false;
  while (index < s.length) {
    currentChar = s.charCodeAt(index);
    if (currentChar === 43 /* '+' */ && decodeSpaces) {
      out[outIndex++] = 32; // ' '
      index++;
      continue;
    }
    if (currentChar === 37 /* '%' */ && index < maxLength) {
      currentChar = s.charCodeAt(++index);
      hexHigh = unhexTable[currentChar];
      if (!(hexHigh >= 0)) {
        out[outIndex++] = 37; // '%'
        continue;
      } else {
        nextChar = s.charCodeAt(++index);
        hexLow = unhexTable[nextChar];
        if (!(hexLow >= 0)) {
          out[outIndex++] = 37; // '%'
          index--;
        } else {
          hasHex = true;
          currentChar = hexHigh * 16 + hexLow;
        }
      }
    }
    out[outIndex++] = currentChar;
    index++;
  }
  return hasHex ? out.slice(0, outIndex) : out;
}

function qsUnescape(s, decodeSpaces) {
  try {
    return decodeURIComponent(s);
  } catch {
    return QueryString.unescapeBuffer(s, decodeSpaces).toString();
  }
}

// Characters that do not need escaping when generating query strings.
const noEscape = new Int8Array([
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0 - 15
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 16 - 31
  0, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 1, 1, 0, // 32 - 47
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, // 48 - 63
  0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 64 - 79
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1, // 80 - 95
  0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 96 - 111
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, // 112 - 127
]);

function qsEscape(str) {
  if (typeof str !== 'string') {
    if (typeof str === 'object') str = String(str);
    else str += '';
  }
  return encodeStr(str, noEscape, hexTable);
}

function stringifyPrimitive(v) {
  if (typeof v === 'string') return v;
  if (typeof v === 'number' && Number.isFinite(v)) return '' + v;
  if (typeof v === 'bigint') return '' + v;
  if (typeof v === 'boolean') return v ? 'true' : 'false';
  return '';
}

function encodeStringified(v, encode) {
  if (typeof v === 'string') return v.length ? encode(v) : '';
  if (typeof v === 'number' && Number.isFinite(v)) {
    return Math.abs(v) < 1e21 ? '' + v : encode('' + v);
  }
  if (typeof v === 'bigint') return '' + v;
  if (typeof v === 'boolean') return v ? 'true' : 'false';
  return '';
}

function encodeStringifiedCustom(v, encode) {
  return encode(stringifyPrimitive(v));
}

function stringify(obj, sep, eq, options) {
  sep ||= '&';
  eq ||= '=';

  let encode = QueryString.escape;
  if (options && typeof options.encodeURIComponent === 'function') {
    encode = options.encodeURIComponent;
  }
  const convert = encode === qsEscape ? encodeStringified : encodeStringifiedCustom;

  if (obj !== null && typeof obj === 'object') {
    const keys = Object.keys(obj);
    const len = keys.length;
    let fields = '';
    for (let i = 0; i < len; ++i) {
      const k = keys[i];
      const v = obj[k];
      let ks = convert(k, encode);
      ks += eq;

      if (Array.isArray(v)) {
        const vlen = v.length;
        if (vlen === 0) continue;
        if (fields) fields += sep;
        for (let j = 0; j < vlen; ++j) {
          if (j) fields += sep;
          fields += ks;
          fields += convert(v[j], encode);
        }
      } else {
        if (fields) fields += sep;
        fields += ks;
        fields += convert(v, encode);
      }
    }
    return fields;
  }
  return '';
}

function charCodes(str) {
  if (str.length === 0) return [];
  if (str.length === 1) return [str.charCodeAt(0)];
  const ret = new Array(str.length);
  for (let i = 0; i < str.length; ++i) ret[i] = str.charCodeAt(i);
  return ret;
}
const defSepCodes = [38]; // &
const defEqCodes = [61]; // =

function addKeyVal(obj, key, value, keyEncoded, valEncoded, decode) {
  if (key.length > 0 && keyEncoded) key = decodeStr(key, decode);
  if (value.length > 0 && valEncoded) value = decodeStr(value, decode);

  if (obj[key] === undefined) {
    obj[key] = value;
  } else {
    const curValue = obj[key];
    // Array-specific property check distinguishes from a string value; safe
    // since we generate all of the values being assigned.
    if (curValue.pop) curValue[curValue.length] = value;
    else obj[key] = [curValue, value];
  }
}

function parseSimple(qs, obj, pairs) {
  let pairStart = 0;
  let keyEnd = -1;

  for (let i = 0; i < qs.length; ++i) {
    const code = qs.charCodeAt(i);
    if (code === 61 /* = */) {
      if (keyEnd < pairStart) keyEnd = i;
    } else if (code === 38 /* & */) {
      if (pairStart < i) {
        if (keyEnd < pairStart) {
          addKeyVal(obj, qs.slice(pairStart, i), '', false, false, qsUnescape);
        } else {
          addKeyVal(
            obj,
            qs.slice(pairStart, keyEnd),
            qs.slice(keyEnd + 1, i),
            false,
            false,
            qsUnescape,
          );
        }
      }
      if (--pairs === 0) return obj;
      pairStart = i + 1;
      keyEnd = -1;
    }
  }

  if (pairStart < qs.length) {
    if (keyEnd < pairStart) {
      addKeyVal(obj, qs.slice(pairStart), '', false, false, qsUnescape);
    } else {
      addKeyVal(
        obj,
        qs.slice(pairStart, keyEnd),
        qs.slice(keyEnd + 1),
        false,
        false,
        qsUnescape,
      );
    }
  }

  return obj;
}

function parse(qs, sep, eq, options) {
  const obj = { __proto__: null };

  if (typeof qs !== 'string' || qs.length === 0) {
    return obj;
  }

  const sepCodes = !sep ? defSepCodes : charCodes(String(sep));
  const eqCodes = !eq ? defEqCodes : charCodes(String(eq));
  const sepLen = sepCodes.length;
  const eqLen = eqCodes.length;

  let pairs = 1000;
  if (options && typeof options.maxKeys === 'number') {
    // -1 means "unlimited" (decremented + checked against 0).
    pairs = options.maxKeys > 0 && Number.isFinite(options.maxKeys) ?
      Math.floor(options.maxKeys) : -1;
  }

  let decode = QueryString.unescape;
  if (options && typeof options.decodeURIComponent === 'function') {
    decode = options.decodeURIComponent;
  }
  const customDecode = decode !== qsUnescape;

  if (
    !customDecode &&
    sepLen === 1 &&
    eqLen === 1 &&
    sepCodes[0] === 38 /* & */ &&
    eqCodes[0] === 61 /* = */ &&
    qs.indexOf('%') === -1 &&
    qs.indexOf('+') === -1
  ) {
    return parseSimple(qs, obj, pairs);
  }

  let lastPos = 0;
  let sepIdx = 0;
  let eqIdx = 0;
  let key = '';
  let value = '';
  let keyEncoded = customDecode;
  let valEncoded = customDecode;
  const plusChar = customDecode ? '%20' : ' ';
  let encodeCheck = 0;
  for (let i = 0; i < qs.length; ++i) {
    const code = qs.charCodeAt(i);

    // Try matching key/value pair separator (e.g. '&')
    if (code === sepCodes[sepIdx]) {
      if (++sepIdx === sepLen) {
        const end = i - sepIdx + 1;
        if (eqIdx < eqLen) {
          if (lastPos < end) {
            key += qs.slice(lastPos, end);
          } else if (key.length === 0) {
            // Empty substring between separators.
            if (--pairs === 0) return obj;
            lastPos = i + 1;
            sepIdx = eqIdx = 0;
            continue;
          }
        } else if (lastPos < end) {
          value += qs.slice(lastPos, end);
        }

        addKeyVal(obj, key, value, keyEncoded, valEncoded, decode);

        if (--pairs === 0) return obj;
        keyEncoded = valEncoded = customDecode;
        key = value = '';
        encodeCheck = 0;
        lastPos = i + 1;
        sepIdx = eqIdx = 0;
      }
    } else {
      sepIdx = 0;
      if (eqIdx < eqLen) {
        if (code === eqCodes[eqIdx]) {
          if (++eqIdx === eqLen) {
            const end = i - eqIdx + 1;
            if (lastPos < end) key += qs.slice(lastPos, end);
            encodeCheck = 0;
            lastPos = i + 1;
          }
          continue;
        } else {
          eqIdx = 0;
          if (!keyEncoded) {
            if (code === 37 /* % */) {
              encodeCheck = 1;
              continue;
            } else if (encodeCheck > 0) {
              if (isHexTable[code] === 1) {
                if (++encodeCheck === 3) keyEncoded = true;
                continue;
              } else {
                encodeCheck = 0;
              }
            }
          }
        }
        if (code === 43 /* + */) {
          if (lastPos < i) key += qs.slice(lastPos, i);
          key += plusChar;
          lastPos = i + 1;
          continue;
        }
      }
      if (code === 43 /* + */) {
        if (lastPos < i) value += qs.slice(lastPos, i);
        value += plusChar;
        lastPos = i + 1;
      } else if (!valEncoded) {
        if (code === 37 /* % */) {
          encodeCheck = 1;
        } else if (encodeCheck > 0) {
          if (isHexTable[code] === 1) {
            if (++encodeCheck === 3) valEncoded = true;
          } else {
            encodeCheck = 0;
          }
        }
      }
    }
  }

  // Deal with any leftover key or value data.
  if (lastPos < qs.length) {
    if (eqIdx < eqLen) key += qs.slice(lastPos);
    else if (sepIdx < sepLen) value += qs.slice(lastPos);
  } else if (eqIdx === 0 && key.length === 0) {
    // Ended on an empty substring.
    return obj;
  }

  addKeyVal(obj, key, value, keyEncoded, valEncoded, decode);

  return obj;
}

function decodeStr(s, decoder) {
  try {
    return decoder(s);
  } catch {
    return QueryString.unescape(s, true);
  }
}

const QueryString = module.exports = {
  unescapeBuffer,
  unescape: qsUnescape,
  escape: qsEscape,
  stringify,
  encode: stringify,
  parse,
  decode: parse,
};
