'use strict';
// `node:child_process` — the event surface over the native spawn primitives.
//
// The native half starts a child and reports its outcome a turn later, and
// owns the channel a forked child joins; this side owns the `ChildProcess`
// class and the table the native dispatchers route through.

const native = require('__cpnative');
const { Buffer } = require('buffer');
const EventEmitter = require('events');
const { Readable, Writable } = require('stream');

function normalizeArgs(command, args, options) {
  if (!Array.isArray(args)) { options = args; args = []; }
  return { command: String(command), args: (args || []).map(String), options: options || {} };
}

function rawSpawn(command, args, options) {
  let input;
  if (options.input !== undefined && options.input !== null) {
    const b = Buffer.isBuffer(options.input) ? options.input : Buffer.from(String(options.input));
    input = b.toString('latin1');
  }
  return native.spawnSyncRaw(command, args, {
    cwd: options.cwd ? String(options.cwd) : undefined,
    input,
    env: options.env,
  });
}

function buildError(raw, command) {
  const e = new Error(raw.error);
  e.code = raw.errorCode;
  e.errno = -2;
  e.syscall = `spawn ${command}`;
  e.path = command;
  e.spawnargs = [];
  return e;
}

function spawnSync(command, args, options) {
  const n = normalizeArgs(command, args, options);
  const raw = rawSpawn(n.command, n.args, n.options);
  const enc = n.options.encoding;
  const decode = (s) => {
    const b = Buffer.from(s, 'latin1');
    return enc && enc !== 'buffer' ? b.toString(enc) : b;
  };
  const stdout = raw.error ? null : decode(raw.stdout);
  const stderr = raw.error ? null : decode(raw.stderr);
  const result = {
    pid: raw.pid,
    output: [null, stdout, stderr],
    stdout,
    stderr,
    status: raw.status,
    signal: raw.signal,
  };
  if (raw.error) result.error = buildError(raw, n.command);
  return result;
}

function checkSyncResult(result, command) {
  if (result.error) throw result.error;
  if (result.status !== 0 && result.status !== null) {
    const e = new Error(`Command failed: ${command}` + (result.stderr ? `\n${result.stderr.toString()}` : ''));
    e.status = result.status;
    e.signal = result.signal;
    e.output = result.output;
    e.pid = result.pid;
    e.stdout = result.stdout;
    e.stderr = result.stderr;
    throw e;
  }
  return result.stdout;
}

function execFileSync(file, args, options) {
  const n = normalizeArgs(file, args, options);
  const result = spawnSync(n.command, n.args, n.options);
  return checkSyncResult(result, n.command);
}

function execSync(command, options) {
  options = options || {};
  const shell = typeof options.shell === 'string' ? options.shell : '/bin/sh';
  const result = spawnSync(shell, ['-c', String(command)], options);
  return checkSyncResult(result, command);
}


// Node names a property `options.x` and an argument `"x"`, and renders the
// offending value the same way in both.
function receivedTail(value) {
  if (value === null || value === undefined) return ` Received ${value}`;
  if (typeof value === 'string') return ` Received type string ('${value}')`;
  if (typeof value === 'function') return ` Received function ${value.name}`;
  if (typeof value === 'object') {
    return ` Received an instance of ${value.constructor ? value.constructor.name : 'Object'}`;
  }
  return ` Received type ${typeof value} (${String(value)})`;
}

function invalidArgType(name, expectation, value, isProperty = false) {
  const subject = isProperty ? `The "${name}" property must be` : `The "${name}" argument must be`;
  const err = new TypeError(`${subject} ${expectation}.${receivedTail(value)}`);
  err.code = 'ERR_INVALID_ARG_TYPE';
  return err;
}

// `envPairs` is a list of `KEY=VALUE` strings, which is the shape the platform
// takes; the rest of this module works with an object.
function envFromPairs(pairs) {
  if (!Array.isArray(pairs)) return undefined;
  const env = {};
  for (const pair of pairs) {
    const text = String(pair);
    const split = text.indexOf('=');
    if (split === -1) continue;
    env[text.slice(0, split)] = text.slice(split + 1);
  }
  return env;
}

const KNOWN_SIGNALS = [
  'SIGHUP', 'SIGINT', 'SIGQUIT', 'SIGILL', 'SIGTRAP', 'SIGABRT', 'SIGIOT', 'SIGBUS',
  'SIGFPE', 'SIGKILL', 'SIGUSR1', 'SIGSEGV', 'SIGUSR2', 'SIGPIPE', 'SIGALRM', 'SIGTERM',
  'SIGCHLD', 'SIGCONT', 'SIGSTOP', 'SIGTSTP', 'SIGTTIN', 'SIGTTOU', 'SIGURG', 'SIGXCPU',
  'SIGXFSZ', 'SIGVTALRM', 'SIGPROF', 'SIGWINCH', 'SIGIO', 'SIGPOLL', 'SIGSYS',
];

// Every started child, by the handle the native half dispatches through.
const children = new Map();

// What marks a message as belonging to a module rather than to the program.
const INTERNAL_PREFIX = 'NODE_';

// `stdio` names what happens to each standard stream, and may carry an `ipc`
// slot — which is how a caller asks `spawn` for a channel, the same one `fork`
// opens by default.
function normalizeStdio(stdio, fallback) {
  const named = stdio === undefined ? fallback : stdio;
  const list = Array.isArray(named) ? named : [named, named, named];
  const streams = [];
  let wantsChannel = false;
  for (const entry of list) {
    if (entry === 'ipc') { wantsChannel = true; continue; }
    if (streams.length < 3) streams.push(entry === undefined ? 'pipe' : String(entry));
  }
  while (streams.length < 3) streams.push('pipe');
  return { streams, wantsChannel };
}

function isInternal(message) {
  return message !== null && typeof message === 'object' &&
    typeof message.cmd === 'string' && message.cmd.startsWith(INTERNAL_PREFIX);
}

class ChildProcess extends EventEmitter {
  constructor() {
    super();
    this.pid = undefined;
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this.connected = false;
    this.channel = null;
    this._handle = 0;
    this.stdout = new Readable({ read() {} });
    this.stderr = new Readable({ read() {} });
    this.stdin = new Writable({ write(c, e, cb) { cb(); } });
    this.stdio = [this.stdin, this.stdout, this.stderr];
    this._piped = [true, true, true];
  }
  // The low-level entry point Node exposes on the class itself. Its argument
  // checks run before anything is spawned, and its tests assert them verbatim.
  spawn(options) {
    if (options === null || typeof options !== 'object') {
      throw invalidArgType('options', 'of type object', options);
    }
    // Node checks the list-shaped properties before the file, and its tests
    // pass one at a time with no file at all.
    if (options.envPairs !== undefined && !Array.isArray(options.envPairs)) {
      throw invalidArgType('options.envPairs', 'an instance of Array', options.envPairs, true);
    }
    if (options.args !== undefined && !Array.isArray(options.args)) {
      throw invalidArgType('options.args', 'an instance of Array', options.args, true);
    }
    if (typeof options.file !== 'string') {
      throw invalidArgType('options.file', 'of type string', options.file, true);
    }
    const args = options.args ? options.args.slice(1) : [];
    this._run(options.file, args, {
      cwd: options.cwd,
      env: envFromPairs(options.envPairs),
    });
    return 0;
  }

  kill(signal) {
    const name = signal === undefined ? 'SIGTERM' : signal;
    // An unknown signal name is refused before anything is sent, which is what
    // Node does and what its tests assert.
    if (typeof name === 'string' && !KNOWN_SIGNALS.includes(name)) {
      const err = new TypeError(`Unknown signal: ${name}`);
      err.code = 'ERR_UNKNOWN_SIGNAL';
      throw err;
    }
    this.killed = true;
    if (typeof this.pid === 'number') {
      try {
        process.kill(this.pid, name);
      } catch {
        // The child may already be gone; `killed` still reflects the request.
      }
    }
    // The child's own exit is reported when it happens, so nothing is
    // announced here on its behalf.
    return true;
  }
  ref() {}
  unref() {}

  // Explicit resource management: disposing a child kills it.
  [Symbol.dispose]() {
    if (!this.killed) this.kill('SIGTERM');
  }

  async [Symbol.asyncDispose]() {
    if (!this.killed) this.kill('SIGTERM');
  }

  send(message, callback) {
    if (arguments.length === 0) {
      const err = new TypeError('The "message" argument must be specified');
      err.code = 'ERR_MISSING_ARGS';
      throw err;
    }
    if (!this.connected) {
      const err = new Error('Channel closed');
      err.code = 'ERR_IPC_CHANNEL_CLOSED';
      if (typeof callback === 'function') { callback(err); return false; }
      throw err;
    }
    let payload;
    try {
      payload = JSON.stringify(message);
    } catch (error) {
      if (typeof callback === 'function') { callback(error); return false; }
      throw error;
    }
    if (payload === undefined) {
      throw invalidArgType(
        'message', 'one of type string, object, number, boolean, or null', message);
    }
    const accepted = native.ipcSend(this._handle, payload);
    if (typeof callback === 'function') {
      setTimeout(() => callback(accepted ? null : new Error('Channel closed')), 0);
    }
    return accepted;
  }

  disconnect() {
    if (!this.connected) return;
    this.connected = false;
    this.channel = null;
    native.ipcDisconnect(this._handle);
    setTimeout(() => this.emit('disconnect'), 0);
  }
  _run(command, args, options) {
    // A stream the child was not given a pipe for is not a stream this side
    // can read, and Node reports that as `null` rather than as a stream that
    // never yields anything.
    const streams = options?.stdio;
    if (Array.isArray(streams)) {
      this._piped = [0, 1, 2].map((slot) => streams[slot] === 'pipe');
      if (!this._piped[1]) this.stdout = null;
      if (!this._piped[2]) this.stderr = null;
      this.stdio = [this.stdin, this.stdout, this.stderr];
    }
    // The child starts now, so `pid` is readable the moment `spawn` returns;
    // its outcome arrives on a later turn through `__otterChildExit`.
    let started;
    try {
      started = native.spawnStart(command, args, options ?? {});
    } catch (error) {
      setTimeout(() => this._failed(error), 0);
      return;
    }
    if (started.error) {
      const raw = started;
      setTimeout(() => this._failed(buildError(raw, command)), 0);
      return;
    }
    this.pid = started.pid;
    this._handle = started.id;
    children.set(started.id, this);
  }

  _failed(error) {
    this.emit('error', error);
    this._endStreams();
    setTimeout(() => this.emit('close', null, null), 0);
  }

  _endStreams() {
    if (this.stdout) this.stdout.push(null);
    if (this.stderr) this.stderr.push(null);
  }

  // The native half reports the outcome once, when the child has run to
  // completion and its output has been read to the end.
  _exited(status, signal, stdout, stderr) {
    children.delete(this._handle);
    if (stdout && this.stdout) this.stdout.push(Buffer.from(stdout, 'latin1'));
    if (stderr && this.stderr) this.stderr.push(Buffer.from(stderr, 'latin1'));
    this._endStreams();
    this.exitCode = status;
    this.signalCode = signal;
    this.connected = false;
    this.channel = null;
    this.emit('exit', status, signal);
    setTimeout(() => this.emit('close', status, signal), 0);
  }

  _channelEvent(kind, payload) {
    if (kind === 'message') {
      let message;
      try {
        message = JSON.parse(payload);
      } catch {
        return;
      }
      // A module built on the channel coordinates with its peer over the same
      // channel the program uses, so its own traffic is reported separately
      // and a program's `message` listeners only see what the peer sent.
      this.emit(isInternal(message) ? 'internalMessage' : 'message', message);
      return;
    }
    if (!this.connected) return;
    this.connected = false;
    this.channel = null;
    this.emit('disconnect');
  }
}

// The native half dispatches here, on the isolate thread.
globalThis.__otterChildExit = function exited(handle, status, signal, stdout, stderr) {
  const child = children.get(handle);
  if (child === undefined) return;
  child._exited(status, signal, stdout, stderr);
};

globalThis.__otterChildIpc = function channelEvent(handle, kind, payload) {
  const child = children.get(handle);
  if (child === undefined) return;
  child._channelEvent(kind, payload);
};

function spawn(command, args, options) {
  const n = normalizeArgs(command, args, options);
  const { streams, wantsChannel } = normalizeStdio(n.options.stdio, ['ignore', 'pipe', 'pipe']);
  const cp = new ChildProcess();
  cp._run(n.command, n.args, { ...n.options, stdio: streams, ipc: wantsChannel });
  if (wantsChannel && cp._handle !== 0) {
    cp.connected = true;
    cp.channel = { ref() {}, unref() {} };
  }
  return cp;
}

function collect(cp, options, cb) {
  const enc = options.encoding === undefined ? 'utf8' : options.encoding;
  const out = []; const err = [];
  cp.stdout.on('data', (d) => out.push(Buffer.isBuffer(d) ? d : Buffer.from(d)));
  cp.stderr.on('data', (d) => err.push(Buffer.isBuffer(d) ? d : Buffer.from(d)));
  cp.on('error', (e) => { if (cb) cb(e, decodeAll(out, enc), decodeAll(err, enc)); cb = null; });
  cp.on('close', (status, signal) => {
    if (!cb) return;
    const stdout = decodeAll(out, enc); const stderr = decodeAll(err, enc);
    if (status !== 0 && status !== null) {
      const e = new Error(`Command failed`);
      e.code = status; e.killed = false; e.signal = signal;
      cb(e, stdout, stderr);
    } else {
      cb(null, stdout, stderr);
    }
  });
}

function decodeAll(chunks, enc) {
  const b = Buffer.concat(chunks);
  return enc && enc !== 'buffer' ? b.toString(enc) : b;
}

function execFile(file, args, options, cb) {
  if (typeof args === 'function') { cb = args; args = []; options = {}; }
  else if (typeof options === 'function') { cb = options; options = {}; }
  const n = normalizeArgs(file, args, options || {});
  const cp = spawn(n.command, n.args, n.options);
  collect(cp, n.options, cb);
  return cp;
}

function exec(command, options, cb) {
  if (typeof options === 'function') { cb = options; options = {}; }
  options = options || {};
  const shell = typeof options.shell === 'string' ? options.shell : '/bin/sh';
  return execFile(shell, ['-c', String(command)], options, cb);
}

// A forked child runs this same binary and joins a channel opened for it, so
// `child.send` here and `process.send` there are the two ends of one channel.
function fork(modulePath, args, options) {
  const a = Array.isArray(args) ? args : [];
  const given = options || {};
  // A forked child shares this process's output unless the caller asked for it
  // on a stream of its own, which is what `silent` means. Either way it gets a
  // channel, which is what makes it a fork rather than a spawn.
  const inherited = given.silent ? 'pipe' : 'inherit';
  const stdio = given.stdio ?? [inherited, inherited, inherited];
  const settings = { ...given, stdio: [...(Array.isArray(stdio) ? stdio : [stdio, stdio, stdio]), 'ipc'] };
  const execPath = (typeof process !== 'undefined' && process.execPath) || 'node';
  return spawn(execPath, [String(modulePath), ...a.map(String)], settings);
}

module.exports = {
  spawn, spawnSync, exec, execSync, execFile, execFileSync, fork, ChildProcess,
};

// Host-dispatch hooks stay off the enumerable global surface: Node's
// test harness treats any enumerable global it does not know as a leak.
Object.defineProperty(globalThis, '__otterChildExit', { enumerable: false });
Object.defineProperty(globalThis, '__otterChildIpc', { enumerable: false });
