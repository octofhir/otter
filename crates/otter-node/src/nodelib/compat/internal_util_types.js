'use strict';
// `internal/util/types` — the hosted util module already carries the type
// checks; the vendored files only add a few aliases on top.
const { types } = require('util');

module.exports = {
  ...types,
  isArrayBufferView: ArrayBuffer.isView,
  isUint8Array(value) {
    return value instanceof Uint8Array;
  },
  isBlob(value) {
    return typeof Blob === 'function' && value instanceof Blob;
  },
};
