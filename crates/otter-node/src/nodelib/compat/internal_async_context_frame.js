'use strict';
// The ambient async-context frame; the engine carries the real context, so
// callbacks bound "to the current frame" just run as-is.
module.exports = {
  current() { return undefined; },
  bind(fn) { return fn; },
  exchange() { return undefined; },
  restore() {},
  bindContext(fn) { return fn; },
};
