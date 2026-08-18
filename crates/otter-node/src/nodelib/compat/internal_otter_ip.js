'use strict';

// Shared textual IP parsers for the compat net stack. v4 parses to a
// 32-bit integer, v6 to a 128-bit BigInt; embedded v4 tails and zone
// suffixes follow the kernel's textual conventions.

function parseV4(addr) {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(String(addr));
  if (m === null) return null;
  let out = 0;
  for (let i = 1; i <= 4; i++) {
    const part = Number(m[i]);
    if (part > 255) return null;
    out = out * 256 + part;
  }
  return out;
}

function parseV6(addr) {
  let text = String(addr).toLowerCase();
  if (text.startsWith('[') && text.endsWith(']')) text = text.slice(1, -1);
  const zone = text.indexOf('%');
  if (zone !== -1) text = text.slice(0, zone);
  let head = text;
  let tail = '';
  const gap = text.indexOf('::');
  if (gap !== -1) {
    head = text.slice(0, gap);
    tail = text.slice(gap + 2);
    if (tail.includes('::')) return null;
  }
  const expand = (part) => (part === '' ? [] : part.split(':'));
  const headParts = expand(head);
  const tailParts = expand(tail);
  const last = tailParts.length > 0 ? tailParts[tailParts.length - 1]
    : headParts[headParts.length - 1];
  if (last !== undefined && last.includes('.')) {
    const v4 = parseV4(last);
    if (v4 === null) return null;
    const words = [(v4 >>> 16).toString(16), (v4 & 0xffff).toString(16)];
    if (tailParts.length > 0) tailParts.splice(-1, 1, ...words);
    else headParts.splice(-1, 1, ...words);
  }
  const missing = 8 - headParts.length - tailParts.length;
  if (gap === -1 ? missing !== 0 : missing < 0) return null;
  const words = [...headParts, ...new Array(gap === -1 ? 0 : missing).fill('0'), ...tailParts];
  if (words.length !== 8) return null;
  let out = 0n;
  for (const word of words) {
    if (!/^[0-9a-f]{1,4}$/.test(word)) return null;
    out = (out << 16n) | BigInt(parseInt(word, 16));
  }
  return out;
}

module.exports = { parseV4, parseV6 };
