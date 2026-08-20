'use strict';

// internal/webstreams/writablestream — see the readable side; the stream is
// the realm's own.

const { WritableStream } = globalThis;

function isWritableStream(value) {
  return value instanceof WritableStream;
}

module.exports = { WritableStream, isWritableStream };
