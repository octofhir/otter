'use strict';
// `node:child_process` — built on the native synchronous spawn primitive
// (`__cpnative.spawnSyncRaw`). The async surface runs the same primitive and
// replays its output through EventEmitter/stream, which is sufficient for the
// common "spawn a child, collect its output, observe exit" pattern.

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

class ChildProcess extends EventEmitter {
  constructor() {
    super();
    this.pid = undefined;
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this.connected = false;
    this.stdout = new Readable({ read() {} });
    this.stderr = new Readable({ read() {} });
    this.stdin = new Writable({ write(c, e, cb) { cb(); } });
    this.stdio = [this.stdin, this.stdout, this.stderr];
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
    this.emit('exit', null, typeof name === 'string' ? name : 'SIGTERM');
    return true;
  }
  ref() {}
  unref() {}
  disconnect() { this.connected = false; }
  _run(command, args, options) {
    // The child starts now, so `pid` is readable the moment `spawn` returns;
    // only its outcome waits for a turn of the loop.
    let started;
    try {
      started = native.spawnStart(command, args, options ?? {});
    } catch (error) {
      setTimeout(() => {
        this.emit('error', error);
        this.stdout.push(null);
        this.stderr.push(null);
        setTimeout(() => this.emit('close', null, null), 0);
      }, 0);
      return;
    }
    if (started.error) {
      const raw = started;
      setTimeout(() => {
        this.emit('error', buildError(raw, command));
        this.stdout.push(null);
        this.stderr.push(null);
        setTimeout(() => this.emit('close', null, null), 0);
      }, 0);
      return;
    }
    this.pid = started.pid;
    setTimeout(() => {
      const raw = native.spawnCollect(this.pid);
      if (raw.error) {
        this.emit('error', buildError(raw, command));
        this.stdout.push(null);
        this.stderr.push(null);
        setTimeout(() => this.emit('close', null, null), 0);
        return;
      }
      if (raw.stdout) this.stdout.push(Buffer.from(raw.stdout, 'latin1'));
      if (raw.stderr) this.stderr.push(Buffer.from(raw.stderr, 'latin1'));
      this.stdout.push(null);
      this.stderr.push(null);
      this.exitCode = raw.status;
      this.signalCode = raw.signal;
      this.emit('exit', raw.status, raw.signal);
      setTimeout(() => this.emit('close', raw.status, raw.signal));
    }, 0);
  }
}

function spawn(command, args, options) {
  const n = normalizeArgs(command, args, options);
  const cp = new ChildProcess();
  cp._run(n.command, n.args, n.options);
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

function fork(modulePath, args, options) {
  const a = Array.isArray(args) ? args : [];
  const execPath = (typeof process !== 'undefined' && process.execPath) || 'node';
  const cp = spawn(execPath, [String(modulePath), ...a.map(String)], options || {});
  cp.connected = true;
  cp.send = () => true;
  return cp;
}

module.exports = {
  spawn, spawnSync, exec, execSync, execFile, execFileSync, fork, ChildProcess,
};
