'use strict';
// internal/buffer — the pieces vendored files reach for. `FastBuffer` is
// Node's `class FastBuffer extends Uint8Array`, constructed the same three
// ways: empty, by length, and over existing memory.
const { Buffer } = require('buffer');

function FastBuffer(arrayBuffer, byteOffset, length) {
  if (arrayBuffer === undefined) return Buffer.alloc(0);
  if (typeof arrayBuffer === 'number') return Buffer.alloc(arrayBuffer);
  return Buffer.from(arrayBuffer, byteOffset, length);
}

module.exports = { FastBuffer };
