'use strict';

// internal/webstreams/readablestream — the standard stream, which this realm
// already has as a global. Node's own libraries reach it through this name.

const { ReadableStream } = globalThis;

function isReadableStream(value) {
  return value instanceof ReadableStream;
}

module.exports = { ReadableStream, isReadableStream };
