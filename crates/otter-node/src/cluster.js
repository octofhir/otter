'use strict';
// `node:cluster` — minimal stub. Single-process model: this is always the
// primary and there are no workers.
const EventEmitter = require('events');

const cluster = new EventEmitter();
cluster.isPrimary = true;
cluster.isMaster = true;
cluster.isWorker = false;
cluster.worker = undefined;
cluster.workers = {};
cluster.settings = {};
cluster.SCHED_NONE = 1;
cluster.SCHED_RR = 2;
cluster.schedulingPolicy = 2;
cluster.setupPrimary = function setupPrimary(settings) { cluster.settings = settings || {}; };
cluster.setupMaster = cluster.setupPrimary;
cluster.fork = function fork() {
  const err = new Error('cluster.fork is not supported in this runtime');
  err.code = 'ERR_UNSUPPORTED';
  throw err;
};
cluster.disconnect = function disconnect(cb) { if (typeof cb === 'function') setTimeout(cb, 0); };

module.exports = cluster;
