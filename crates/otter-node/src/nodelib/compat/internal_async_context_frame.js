'use strict';
// The ambient async-context frame; the engine carries the real context, so
// callbacks bound "to the current frame" just run as-is.
module.exports = {
  current() { return undefined; },
  bind(fn) { return fn; },
  exchange() { return undefined; },
  // Putting a frame back after a callback has run. The engine carries the real
  // context across its own boundaries, so there is nothing here to put back.
  set() {},
  restore() {},
  bindContext(fn) { return fn; },
};
