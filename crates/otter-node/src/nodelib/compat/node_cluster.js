'use strict';
// `node:cluster` — the vendored module plus the step Node performs during
// pre-execution rather than inside the module itself.
//
// A process launched with a worker id sets its worker up before any user
// code runs, and then forgets the id: a process the worker itself launches
// is not a worker.

const cluster = require('internal/otter/cluster');

if (process.env.NODE_UNIQUE_ID) {
  cluster._setupWorker();
  delete process.env.NODE_UNIQUE_ID;
}

module.exports = cluster;
