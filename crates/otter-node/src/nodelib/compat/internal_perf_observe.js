'use strict';
// internal/perf/observe — nothing observes http timing entries here, so the
// gates answer no and the mark/measure hooks are inert.
module.exports = {
  hasObserver() { return false; },
  startPerf() {},
  stopPerf() {},
  enqueue() {},
};
