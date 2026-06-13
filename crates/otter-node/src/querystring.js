'use strict';
// `node:querystring` — classic query-string parse/stringify. Pure JS, no deps.

function qsEscape(str) {
  return encodeURIComponent(String(str));
}

function qsUnescape(str) {
  try {
    return decodeURIComponent(str);
  } catch {
    return str;
  }
}

function unescapeBuffer(s, decodeSpaces) {
  // Minimal percent-decoder returning a byte array (Buffer-like consumers index it).
  const out = [];
  for (let i = 0; i < s.length; i++) {
    let c = s.charCodeAt(i);
    if (c === 0x2b && decodeSpaces) { out.push(0x20); continue; } // '+'
    if (c === 0x25 && i + 2 < s.length) { // '%'
      const hex = s.substr(i + 1, 2);
      const v = parseInt(hex, 16);
      if (!Number.isNaN(v)) { out.push(v); i += 2; continue; }
    }
    out.push(c & 0xff);
  }
  return out;
}

function stringify(obj, sep, eq, options) {
  sep = sep || '&';
  eq = eq || '=';
  const encode = (options && options.encodeURIComponent) || qsEscape;
  if (obj === null || typeof obj !== 'object') return '';
  const keys = Object.keys(obj);
  const parts = [];
  for (const key of keys) {
    const ek = encode(key);
    const value = obj[key];
    if (Array.isArray(value)) {
      for (const v of value) parts.push(`${ek}${eq}${encode(stringifyPrimitive(v))}`);
    } else {
      parts.push(`${ek}${eq}${encode(stringifyPrimitive(value))}`);
    }
  }
  return parts.join(sep);
}

function stringifyPrimitive(v) {
  if (typeof v === 'string') return v;
  if (typeof v === 'number' && Number.isFinite(v)) return String(v);
  if (typeof v === 'boolean') return v ? 'true' : 'false';
  if (typeof v === 'bigint') return String(v);
  return '';
}

function parse(qs, sep, eq, options) {
  sep = sep || '&';
  eq = eq || '=';
  const decode = (options && options.decodeURIComponent) || qsUnescape;
  let maxKeys = 1000;
  if (options && typeof options.maxKeys === 'number') maxKeys = options.maxKeys;

  const result = { __proto__: null };
  if (typeof qs !== 'string' || qs.length === 0) return result;

  const pairs = qs.split(sep);
  const limit = maxKeys > 0 ? Math.min(pairs.length, maxKeys) : pairs.length;
  for (let i = 0; i < limit; i++) {
    const pair = pairs[i].replace(/\+/g, '%20');
    const idx = pair.indexOf(eq);
    let key; let value;
    if (idx >= 0) {
      key = decode(pair.slice(0, idx));
      value = decode(pair.slice(idx + eq.length));
    } else {
      key = decode(pair);
      value = '';
    }
    if (!Object.prototype.hasOwnProperty.call(result, key)) {
      result[key] = value;
    } else if (Array.isArray(result[key])) {
      result[key].push(value);
    } else {
      result[key] = [result[key], value];
    }
  }
  return result;
}

module.exports = {
  parse,
  decode: parse,
  stringify,
  encode: stringify,
  escape: qsEscape,
  unescape: qsUnescape,
  unescapeBuffer,
};
