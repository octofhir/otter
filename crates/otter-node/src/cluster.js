'use strict';
// `node:cluster` — a primary that forks workers and the channel it keeps to
// each one.
//
// The module is the same object in both roles and reports which one it is: a
// process launched with a worker id is a worker, everything else is the
// primary. The two coordinate over the channel `child_process.fork` opens, on
// `NODE_`-prefixed messages, which the channel reports as `internalMessage` so
// this traffic never reaches a program's own `message` listeners.

const EventEmitter = require('events');
const childProcess = require('child_process');

const WORKER_ID_VAR = 'NODE_UNIQUE_ID';
const INTERNAL = 'NODE_CLUSTER';

// A worker learns its id from the environment its primary gave it, and then
// forgets it: a process the worker itself launches is not a worker.
const assignedId = process.env[WORKER_ID_VAR];
delete process.env[WORKER_ID_VAR];

const cluster = new EventEmitter();

cluster.isWorker = assignedId !== undefined;
cluster.isPrimary = !cluster.isWorker;
cluster.isMaster = cluster.isPrimary;
cluster.worker = undefined;
cluster.workers = cluster.isPrimary ? {} : undefined;
cluster.settings = {};
cluster.SCHED_NONE = 1;
cluster.SCHED_RR = 2;
cluster.schedulingPolicy = cluster.SCHED_RR;

function internalMessage(act, extra) {
  return { cmd: INTERNAL, act, ...extra };
}

// One worker, as seen from either side: the primary holds one per child, and a
// worker holds one representing itself.
class Worker extends EventEmitter {
  constructor(options) {
    super();
    this.id = options.id;
    this.process = options.process;
    this.state = options.state ?? 'none';
    this.exitedAfterDisconnect = false;
  }

  send(...args) {
    return this.process.send(...args);
  }

  isConnected() {
    return this.process.connected === true;
  }

  isDead() {
    return this.process.exitCode !== null || this.process.signalCode !== null;
  }

  kill(signal) {
    this.destroy(signal);
  }

  destroy(signal) {
    this.exitedAfterDisconnect = true;
    this.state = 'destroying';
    if (typeof this.process.kill === 'function') this.process.kill(signal ?? 'SIGTERM');
  }

  disconnect() {
    this.exitedAfterDisconnect = true;
    this.state = 'disconnecting';
    if (typeof this.process.disconnect === 'function') this.process.disconnect();
    return this;
  }
}

cluster.Worker = Worker;

// `exec` is the program a worker runs, which is this one unless the primary
// says otherwise.
function defaultSettings(settings) {
  const argv = Array.isArray(process.argv) ? process.argv : [];
  return {
    exec: argv[1],
    args: argv.slice(2),
    execArgv: [],
    silent: false,
    ...(settings ?? {}),
  };
}

cluster.setupPrimary = function setupPrimary(settings) {
  cluster.settings = defaultSettings({ ...cluster.settings, ...(settings ?? {}) });
  cluster.emit('setup', cluster.settings);
  return cluster.settings;
};
cluster.setupMaster = cluster.setupPrimary;

if (cluster.isPrimary) {
  let lastId = 0;

  cluster.fork = function fork(env) {
    cluster.settings = defaultSettings(cluster.settings);
    const id = ++lastId;
    const workerEnv = { ...process.env, ...(env ?? {}), [WORKER_ID_VAR]: String(id) };
    const child = childProcess.fork(cluster.settings.exec, cluster.settings.args, {
      cwd: cluster.settings.cwd,
      env: workerEnv,
      silent: cluster.settings.silent,
    });
    const worker = new Worker({ id, process: child });
    cluster.workers[id] = worker;

    child.on('internalMessage', (message) => {
      if (message.act === 'online') {
        worker.state = 'online';
        worker.emit('online');
        cluster.emit('online', worker);
        return;
      }
      if (message.act === 'listening') {
        worker.state = 'listening';
        const address = message.address ?? {};
        worker.emit('listening', address);
        cluster.emit('listening', worker, address);
      }
    });

    child.on('message', (message) => {
      worker.emit('message', message);
      cluster.emit('message', worker, message);
    });

    child.on('disconnect', () => {
      worker.state = 'disconnected';
      worker.emit('disconnect');
      cluster.emit('disconnect', worker);
    });

    child.on('exit', (code, signal) => {
      worker.state = 'dead';
      delete cluster.workers[id];
      worker.emit('exit', code, signal);
      cluster.emit('exit', worker, code, signal);
    });

    child.on('error', (error) => worker.emit('error', error));

    cluster.emit('fork', worker);
    return worker;
  };

  // Answers once no worker is left, not once each has let go of its end: a
  // worker that has disconnected is still running until it exits, and a
  // primary told to disconnect is waiting for all of them to be gone.
  cluster.disconnect = function disconnect(callback) {
    const workers = Object.values(cluster.workers);
    if (typeof callback === 'function') {
      if (workers.length === 0) {
        setTimeout(callback, 0);
      } else {
        let outstanding = workers.length;
        for (const worker of workers) {
          worker.once('exit', () => {
            outstanding -= 1;
            if (outstanding === 0) callback();
          });
        }
      }
    }
    for (const worker of workers) {
      if (worker.isConnected()) worker.disconnect();
    }
  };
} else {
  const worker = new Worker({
    id: Number(assignedId),
    process,
    state: 'online',
  });
  cluster.worker = worker;

  process.on('message', (message) => {
    worker.emit('message', message);
    cluster.emit('message', worker, message);
  });
  process.on('disconnect', () => {
    worker.state = 'disconnected';
    worker.emit('disconnect');
    cluster.emit('disconnect', worker);
  });

  // The primary counts a worker as online once it says so itself, which is the
  // first thing it does.
  if (typeof process.send === 'function') process.send(internalMessage('online'));

  cluster.fork = function fork() {
    const err = new Error('A worker cannot fork workers');
    err.code = 'ERR_CLUSTER_INVALID_ROLE';
    throw err;
  };
  cluster.disconnect = function disconnect(callback) {
    if (typeof callback === 'function') worker.once('disconnect', callback);
    worker.disconnect();
  };
}

module.exports = cluster;
