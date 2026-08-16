'use strict';
module.exports = {
  AbortController: globalThis.AbortController,
  AbortSignal: globalThis.AbortSignal,
  aborted(signal) { return signal?.aborted === true; },
};
