'use strict';
// internal/buffer — the pieces vendored files reach for. `FastBuffer` is
// Node's `class FastBuffer extends Uint8Array`, Buffer's species, which
// validates nothing.
const { Buffer } = require('buffer');

const FastBuffer = Buffer[Symbol.species];

module.exports = { FastBuffer };
