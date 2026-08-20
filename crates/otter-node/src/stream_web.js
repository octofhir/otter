'use strict';

// `node:stream/web` — the WHATWG streams this realm already has.
//
// There is one implementation of these streams, and it is the standards one
// the page-facing globals come from: a module that answered with classes of
// its own would hand a program streams that its own `instanceof` checks — and
// every adapter written against the standard — would refuse.

module.exports = {
  ReadableStream: globalThis.ReadableStream,
  ReadableStreamDefaultReader: globalThis.ReadableStreamDefaultReader,
  ReadableStreamBYOBReader: globalThis.ReadableStreamBYOBReader,
  ReadableStreamBYOBRequest: globalThis.ReadableStreamBYOBRequest,
  ReadableByteStreamController: globalThis.ReadableByteStreamController,
  ReadableStreamDefaultController: globalThis.ReadableStreamDefaultController,
  TransformStream: globalThis.TransformStream,
  TransformStreamDefaultController: globalThis.TransformStreamDefaultController,
  WritableStream: globalThis.WritableStream,
  WritableStreamDefaultWriter: globalThis.WritableStreamDefaultWriter,
  WritableStreamDefaultController: globalThis.WritableStreamDefaultController,
  ByteLengthQueuingStrategy: globalThis.ByteLengthQueuingStrategy,
  CountQueuingStrategy: globalThis.CountQueuingStrategy,
  TextEncoderStream: globalThis.TextEncoderStream,
  TextDecoderStream: globalThis.TextDecoderStream,
  CompressionStream: globalThis.CompressionStream,
  DecompressionStream: globalThis.DecompressionStream,
};
