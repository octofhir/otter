'use strict';

// internalBinding('cares_wrap') — the textual-address helpers vendored
// `net` needs. Resolver classes stay on the hosted `dns` module.

const { Buffer } = require('buffer');
const { parseV6 } = require('internal/otter/ip');

function convertIpv6StringToBuffer(address) {
  const value = parseV6(address);
  const out = Buffer.alloc(16);
  if (value === null) return out;
  let rest = value;
  for (let i = 15; i >= 0; i--) {
    out[i] = Number(rest & 0xffn);
    rest >>= 8n;
  }
  return out;
}

module.exports = { convertIpv6StringToBuffer };
