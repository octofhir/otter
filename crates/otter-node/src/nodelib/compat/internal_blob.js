'use strict';
module.exports = {
  isBlob(value) { return typeof Blob === 'function' && value instanceof Blob; },
  Blob: globalThis.Blob,
};
