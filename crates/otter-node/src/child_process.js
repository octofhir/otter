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

const MAX_BUFFER = 1024 * 1024;

// The numbers this platform gives its signals. A caller may name a signal or
// number it, and both have to reach the same one.
const SIGNAL_NUMBERS = {
  SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGIOT: 6,
  SIGEMT: 7, SIGFPE: 8, SIGKILL: 9, SIGBUS: 10, SIGSEGV: 11, SIGSYS: 12,
  SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15, SIGURG: 16, SIGSTOP: 17, SIGTSTP: 18,
  SIGCONT: 19, SIGCHLD: 20, SIGTTIN: 21, SIGTTOU: 22, SIGIO: 23, SIGXCPU: 24,
  SIGXFSZ: 25, SIGVTALRM: 26, SIGPROF: 27, SIGWINCH: 28, SIGINFO: 29,
  SIGUSR1: 30, SIGUSR2: 31,
};

// The concatenation a shell run is built on is a deprecation, and a
// deprecation is news once per process rather than once per call.
let shellArgsWarned = false;

function outOfRange(name, expectation, value) {
  const err = new RangeError(
    `The value of "${name}" is out of range. It must be ${expectation}. Received ${value}`);
  err.code = 'ERR_OUT_OF_RANGE';
  return err;
}

function invalidArgValue(name, value, reason, isProperty = false) {
  const kind = isProperty ? 'property' : 'argument';
  const shown = typeof value === 'string' ? `'${value}'` : String(value);
  const err = new TypeError(`The ${kind} '${name}' ${reason}. Received ${shown}`);
  err.code = 'ERR_INVALID_ARG_VALUE';
  return err;
}

// A NUL ends a string for the platform, so a string carrying one would reach
// the child truncated and meaning something else. Node refuses it instead.
function nullByteCheck(value, name, isProperty = false) {
  if (typeof value === 'string' && value.includes('\u0000')) {
    throw invalidArgValue(name, value, 'must be a string without null bytes', isProperty);
  }
}

function nullByteCheckAll(values, name) {
  if (!Array.isArray(values)) return;
  for (let i = 0; i < values.length; i++) nullByteCheck(values[i], `${name}[${i}]`);
}

function validateTimeout(timeout) {
  if (timeout !== undefined && timeout !== null &&
      !(Number.isInteger(timeout) && timeout >= 0)) {
    throw outOfRange('timeout', 'an unsigned integer', timeout);
  }
}

function validateMaxBuffer(maxBuffer) {
  if (maxBuffer !== undefined && maxBuffer !== null &&
      !(typeof maxBuffer === 'number' && maxBuffer >= 0)) {
    throw outOfRange('options.maxBuffer', 'a positive number', maxBuffer);
  }
}

function sanitizeKillSignal(killSignal) {
  if (killSignal === undefined || killSignal === null) return 'SIGTERM';
  if (typeof killSignal === 'number') {
    for (const name of Object.keys(SIGNAL_NUMBERS)) {
      if (SIGNAL_NUMBERS[name] === killSignal) return name;
    }
    const err = new TypeError(`Unknown signal: ${killSignal}`);
    err.code = 'ERR_UNKNOWN_SIGNAL';
    throw err;
  }
  if (typeof killSignal === 'string') {
    if (SIGNAL_NUMBERS[killSignal] === undefined) {
      const err = new TypeError(`Unknown signal: ${killSignal}`);
      err.code = 'ERR_UNKNOWN_SIGNAL';
      throw err;
    }
    return killSignal;
  }
  throw invalidArgType('options.killSignal', 'one of type string or number', killSignal, true);
}

function validateAbortSignal(signal, name) {
  if (signal === undefined || signal === null) return;
  const isSignal = typeof signal === 'object' && typeof signal.addEventListener === 'function' &&
    'aborted' in signal;
  if (!isSignal) throw invalidArgType(name, 'an instance of AbortSignal', signal, true);
}

function abortError(reason) {
  const err = new Error('The operation was aborted');
  err.name = 'AbortError';
  err.code = 'ABORT_ERR';
  if (reason !== undefined) err.cause = reason;
  return err;
}

// The number this platform gives a libuv error name, which is what
// `util.getSystemErrorName` reads back.
function uvErrno(code) {
  const { internalBinding } = require('internal/bootstrap/realm');
  const number = internalBinding('uv')[`UV_${code}`];
  return typeof number === 'number' ? number : -1;
}

// A failure the platform reported, named the way Node names it: the call that
// failed, then the code.
function errnoException(code, syscall) {
  const err = new Error(`${syscall} ${code}`);
  err.errno = uvErrno(code);
  err.code = code;
  err.syscall = syscall;
  return err;
}

// An error that carries a child's outcome rather than a platform code.
function outcomeError(message, outcome) {
  const err = new Error(message);
  for (const key of ['status', 'signal', 'output', 'pid', 'stdout', 'stderr', 'code', 'killed']) {
    if (outcome[key] !== undefined) err[key] = outcome[key];
  }
  return err;
}

// Everything `spawn` and its synchronous twin agree on: what to run, with
// which arguments, and under which of the documented options.
function normalizeSpawnArguments(file, args, options) {
  if (typeof file !== 'string') throw invalidArgType('file', 'of type string', file);
  nullByteCheck(file, 'file');
  if (file.length === 0) throw invalidArgValue('file', file, 'cannot be empty');

  if (Array.isArray(args)) {
    args = args.slice(0);
  } else if (args === undefined || args === null) {
    args = [];
  } else if (typeof args !== 'object') {
    throw invalidArgType('args', 'of type object', args);
  } else {
    options = args;
    args = [];
  }
  nullByteCheckAll(args, 'args');
  // What reaches the platform is text, and each argument becomes it exactly
  // once: an object with a `toString` must not be asked twice.
  args = args.map((arg) => (typeof arg === 'string' ? arg : String(arg)));

  if (options === undefined || options === null) options = {};
  else if (typeof options !== 'object') throw invalidArgType('options', 'of type object', options);

  const cwd = options.cwd;
  if (cwd !== undefined && cwd !== null) {
    if (typeof cwd !== 'string') throw invalidArgType('options.cwd', 'of type string', cwd, true);
    nullByteCheck(cwd, 'options.cwd', true);
  }
  for (const name of ['detached', 'windowsHide', 'windowsVerbatimArguments']) {
    const value = options[name];
    if (value !== undefined && value !== null && typeof value !== 'boolean') {
      throw invalidArgType(`options.${name}`, 'of type boolean', value, true);
    }
  }
  for (const name of ['uid', 'gid']) {
    const value = options[name];
    if (value !== undefined && value !== null &&
        !(Number.isInteger(value) && value >= -2147483648 && value <= 2147483647)) {
      throw invalidArgType(`options.${name}`, 'of type int32', value, true);
    }
  }
  if (options.shell !== undefined && options.shell !== null &&
      typeof options.shell !== 'boolean' && typeof options.shell !== 'string') {
    throw invalidArgType('options.shell', 'one of type boolean or string', options.shell, true);
  }
  nullByteCheck(options.shell, 'options.shell', true);
  if (options.argv0 !== undefined && options.argv0 !== null) {
    if (typeof options.argv0 !== 'string') {
      throw invalidArgType('options.argv0', 'of type string', options.argv0, true);
    }
    nullByteCheck(options.argv0, 'options.argv0', true);
  }

  // What reaches the child's environment is text the platform reads up to its
  // first NUL, so a name or value carrying one would arrive meaning something
  // else.
  if (options.env !== undefined && options.env !== null && typeof options.env === 'object') {
    for (const key of Object.keys(options.env)) {
      const value = options.env[key];
      if (value === undefined) continue;
      nullByteCheck(key, `options.env['${key}']`, true);
      nullByteCheck(typeof value === 'string' ? value : String(value),
                    `options.env['${key}']`, true);
    }
  }

  let argv0 = typeof options.argv0 === 'string' ? options.argv0 : undefined;
  // A shell run is one command line handed to a shell, so the file becomes
  // the shell and everything the caller named becomes its argument.
  if (options.shell) {
    // A shell builds its command line by concatenation, so an argument that
    // carries shell syntax is shell syntax by the time the shell sees it.
    if (args.length > 0 && !shellArgsWarned) {
      shellArgsWarned = true;
      process.emitWarning(
        'Passing args to a child process with shell option true can lead to security ' +
        'vulnerabilities, as the arguments are not escaped, only concatenated.',
        'DeprecationWarning', 'DEP0190');
    }
    const command = [file, ...args].join(' ');
    file = typeof options.shell === 'string' ? options.shell : '/bin/sh';
    args = ['-c', command];
    argv0 = argv0 ?? file;
  }

  return { ...options, file, args, cwd, argv0 };
}

// `execFile(file[, args][, options][, callback])` — everything after the file
// is optional, and a function anywhere in it is the callback.
function normalizeExecFileArgs(file, args, options, callback) {
  if (Array.isArray(args)) {
    args = args.slice(0);
  } else if (typeof args === 'function') {
    callback = args;
    options = undefined;
    args = [];
  } else if (args !== undefined && args !== null && typeof args === 'object') {
    callback = options;
    options = args;
    args = [];
  } else {
    args = [];
  }
  if (typeof options === 'function') {
    callback = options;
    options = undefined;
  } else if (options !== undefined && options !== null && typeof options !== 'object') {
    throw invalidArgType('options', 'of type object', options);
  }
  if (callback !== undefined && callback !== null && typeof callback !== 'function') {
    throw invalidArgType('callback', 'of type function', callback);
  }
  nullByteCheck(options?.argv0, 'options.argv0', true);
  return { file, args, options, callback };
}

// `exec` is `execFile` through a shell: the whole command is one string the
// shell parses, so the file is that string and `shell` is on by default.
function normalizeExecArgs(command, options, callback) {
  nullByteCheck(command, 'command');
  if (typeof options === 'function') {
    callback = options;
    options = undefined;
  }
  nullByteCheck(options?.argv0, 'options.argv0', true);
  options = { ...options };
  options.shell = typeof options.shell === 'string' ? options.shell : true;
  return { file: command, options, callback };
}

function spawnSync(file, args, options) {
  const normalized = normalizeSpawnArguments(file, args, options);
  validateTimeout(normalized.timeout);
  const maxBuffer = normalized.maxBuffer === undefined ? MAX_BUFFER : normalized.maxBuffer;
  validateMaxBuffer(maxBuffer);
  const killSignal = sanitizeKillSignal(normalized.killSignal);

  let input;
  if (normalized.input !== undefined && normalized.input !== null) {
    if (typeof normalized.input === 'string') {
      input = Buffer.from(normalized.input, normalized.encoding === 'buffer'
        ? undefined
        : normalized.encoding);
    } else if (ArrayBuffer.isView(normalized.input)) {
      input = Buffer.from(normalized.input.buffer,
                          normalized.input.byteOffset,
                          normalized.input.byteLength);
    } else {
      throw invalidArgType('options.stdio[0]',
                           'one of type Buffer, TypedArray, DataView, or string',
                           normalized.input, true);
    }
  }

  const { streams } = normalizeStdio(normalized.stdio, ['pipe', 'pipe', 'pipe']);
  const raw = native.spawnSyncRaw(normalized.file, normalized.args, {
    cwd: normalized.cwd === undefined || normalized.cwd === null
      ? undefined
      : String(normalized.cwd),
    argv0: normalized.argv0,
    input: input === undefined ? undefined : input.toString('latin1'),
    env: normalized.env === undefined || normalized.env === null
      ? { ...process.env }
      : normalized.env,
    stdio: streams,
    maxBuffer,
    timeout: normalized.timeout ?? 0,
    killSignal,
  });

  const encoding = normalized.encoding;
  const decode = (view) => {
    if (view === null || view === undefined) return null;
    const bytes = Buffer.from(view);
    return encoding && encoding !== 'buffer' ? bytes.toString(encoding) : bytes;
  };
  const stdout = decode(raw.stdout);
  const stderr = decode(raw.stderr);
  const result = {
    pid: raw.pid,
    output: [null, stdout, stderr],
    stdout,
    stderr,
    status: raw.status,
    signal: raw.signal,
  };
  if (raw.errorCode) {
    result.error = errnoException(raw.errorCode, `spawnSync ${normalized.file}`);
    result.error.path = normalized.file;
    result.error.spawnargs = normalized.args.slice(0);
  }
  return result;
}

// A synchronous run reports failure by throwing, and the throw carries the
// whole outcome — status, signal, and whatever the child managed to say.
function checkExecSyncError(result, args, command) {
  if (!result.error && result.status === 0) return undefined;
  let message = `Command failed: ${command ?? args.join(' ')}`;
  if (result.stderr && result.stderr.length > 0) message += `\n${result.stderr.toString()}`;
  const err = outcomeError(message, result);
  if (result.error) {
    err.code = result.error.code;
    err.errno = result.error.errno;
    err.syscall = result.error.syscall;
    err.path = result.error.path;
    err.spawnargs = result.error.spawnargs;
  }
  return err;
}

function execFileSync(file, args, options) {
  const normalized = normalizeExecFileArgs(file, args, options);
  // Without an explicit `stdio`, the child's diagnostics are the caller's:
  // Node passes them straight through to this process's own error stream.
  const inheritStderr = !normalized.options?.stdio;
  const result = spawnSync(normalized.file, normalized.args, normalized.options);
  if (inheritStderr && result.stderr && result.stderr.length > 0) {
    process.stderr.write(result.stderr);
  }
  const err = checkExecSyncError(
    result, [normalized.options?.argv0 || normalized.file, ...normalized.args]);
  if (err) throw err;
  return result.stdout;
}

function execSync(command, options) {
  const normalized = normalizeExecArgs(command, options, null);
  const inheritStderr = !normalized.options.stdio;
  const result = spawnSync(normalized.file, [], normalized.options);
  if (inheritStderr && result.stderr && result.stderr.length > 0) {
    process.stderr.write(result.stderr);
  }
  const err = checkExecSyncError(result, undefined, command);
  if (err) throw err;
  return result.stdout;
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
    const self = this;
    this.stdin = new Writable({
      write(chunk, encoding, cb) {
        const payload = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), encoding);
        native.childStdinWrite(self._handle, payload);
        cb();
      },
      final(cb) {
        native.childStdinEnd(self._handle);
        cb();
      },
    });
    this.stdio = [this.stdin, this.stdout, this.stderr];
    this._piped = [true, true, true];
    this._streamEnded = [false, false, false];
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

  send(message, sendHandle, options, callback) {
    // Node's argument shuffle: everything after the message is optional and
    // a function anywhere in it is the callback.
    if (typeof sendHandle === 'function') {
      callback = sendHandle; sendHandle = undefined; options = undefined;
    } else if (typeof options === 'function') {
      callback = options; options = undefined;
    }
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
    let prepared;
    try {
      prepared = prepareSend(message, sendHandle, options);
    } catch (error) {
      if (typeof callback === 'function') { callback(error); return false; }
      throw error;
    }
    if (prepared === undefined) {
      throw invalidArgType(
        'message', 'one of type string, object, number, boolean, or null', message);
    }
    const accepted = native.ipcSend(this._handle, prepared.text, prepared.fd);
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
      const opts = { ...(options ?? {}) };
      // Node's child inherits the parent's *JavaScript* environment: a
      // mutation of `process.env` after startup must be visible in the
      // child even though it never reached the host process environment.
      if (opts.env === undefined || opts.env === null) opts.env = { ...process.env };
      started = native.spawnStart(command, args, opts);
    } catch (error) {
      setTimeout(() => this._failed(error), 0);
      return;
    }
    if (started.error) {
      const failure = errnoException(started.errorCode ?? 'EIO', `spawn ${command}`);
      failure.path = command;
      failure.spawnargs = args.slice(0);
      setTimeout(() => this._failed(failure), 0);
      return;
    }
    this.pid = started.pid;
    this._handle = started.id;
    children.set(started.id, this);
    // A child that started is news the caller can act on, and it arrives on
    // the turn after `spawn` returned so the object is theirs first.
    process.nextTick(() => this.emit('spawn'));
  }

  _failed(error) {
    this.emit('error', error);
    this._endStreams();
    setTimeout(() => this.emit('close', null, null), 0);
  }

  _endStreams() {
    if (this.stdout && !this._streamEnded[1]) {
      this._streamEnded[1] = true;
      this.stdout.push(null);
    }
    if (this.stderr && !this._streamEnded[2]) {
      this._streamEnded[2] = true;
      this.stderr.push(null);
    }
  }

  // One live chunk from an output pipe; `null` marks that pipe's end.
  _stdioChunk(which, chunk) {
    const stream = which === 1 ? this.stdout : this.stderr;
    if (!stream) return;
    if (chunk === null) {
      if (!this._streamEnded[which]) {
        this._streamEnded[which] = true;
        stream.push(null);
      }
      return;
    }
    stream.push(Buffer.from(chunk, 'latin1'));
  }

  // The native half reports the outcome once the child has exited and both
  // output pipes reached end-of-stream (their chunks were delivered first).
  _exited(status, signal) {
    children.delete(this._handle);
    this._endStreams();
    this.exitCode = status;
    this.signalCode = signal;
    this.connected = false;
    this.channel = null;
    this.emit('exit', status, signal);
    setTimeout(() => this.emit('close', status, signal), 0);
  }

  _channelEvent(kind, payload, handleFdIn) {
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
      const { event, message: inner, handle } = unwrap(message, handleFdIn);
      if (handle === undefined) {
        this.emit(event, inner);
        return;
      }
      this.emit(event, inner, handle);
      return;
    }
    if (!this.connected) return;
    this.connected = false;
    this.channel = null;
    this.emit('disconnect');
  }
}

// The native half dispatches here, on the isolate thread.
globalThis.__otterChildStdio = function stdioChunk(handle, which, chunk) {
  const child = children.get(handle);
  if (!child) return;
  child._stdioChunk(which, chunk);
};

globalThis.__otterChildExit = function exited(handle, status, signal) {
  const child = children.get(handle);
  if (child === undefined) return;
  child._exited(status, signal);
};

globalThis.__otterChildIpc = function channelEvent(handle, kind, payload, handleFdIn) {
  const child = children.get(handle);
  if (child === undefined) return;
  child._channelEvent(kind, payload, handleFdIn);
};


// ---- handle passing ----
//
// What crosses a channel is a duplicate of a descriptor, so this process
// keeps its own open and the peer owns what it receives. A descriptor alone
// does not say what the sender meant by it — the same listening socket is a
// `net.Server` to one program and a bare handle to `cluster` — so the sender
// names the kind and the receiver builds that. The name rides in an envelope
// around the message, which the receiver unwraps before anyone sees it.

const HANDLE_ENVELOPE = 'NODE_HANDLE';

// What the sender is handing over, or `null` when the message carries only
// itself.
function describeHandle(sendHandle, options) {
  if (sendHandle === undefined || sendHandle === null) return null;
  const dgramHandle = dgramHandleOf(sendHandle);
  const inner = dgramHandle ?? sendHandle._handle ?? sendHandle;
  // A connection names itself by the id the host carries it under; a server
  // names itself by its listener's.
  const id = typeof inner?.fd === 'number' && inner.fd !== -1
    ? inner.fd
    : inner?._serverId;
  if (typeof id !== 'number' || id === -1) return null;
  const { UDP } = require('internal/otter/udp_wrap');
  // A datagram socket lives in its own table, so it is asked for its own
  // duplicate.
  const fd = inner instanceof UDP
    ? require('internal/otter/dgram').dupFd(id)
    : netNative().dupFd(id);
  if (typeof fd !== 'number' || fd < 0) return null;
  return {
    fd,
    type: handleType(sendHandle, dgramHandle),
    dgramType: dgramHandle === undefined || dgramHandle === null
      ? undefined
      : sendHandle.type,
    handle: inner,
    socket: sendHandle,
    keepOpen: options?.keepOpen === true,
  };
}

function handleType(sendHandle, dgramHandle) {
  if (dgramHandle !== undefined && dgramHandle !== null) return 'dgram.Socket';
  const net = require('net');
  if (sendHandle instanceof net.Server) return 'net.Server';
  if (sendHandle instanceof net.Socket) return 'net.Socket';
  return 'net.Native';
}

function envelope(message, carried) {
  return {
    cmd: HANDLE_ENVELOPE,
    type: carried.type,
    dgramType: carried.dgramType,
    msg: message,
  };
}

// A sent connection is a connection this side no longer has: the peer owns
// it now, and two readers on one socket would race for its bytes.
function detachSent(carried) {
  if (carried.type !== 'net.Socket' || carried.keepOpen) return;
  const socket = carried.socket;
  const handle = carried.handle;
  // A connection handed to another process is no longer one this server has:
  // a server waiting to close counts it as gone the moment it leaves.
  if (socket.server !== undefined && socket.server !== null &&
      typeof socket.server._connections === 'number') {
    socket.server._connections--;
  }
  socket._handle = null;
  if (typeof socket.setTimeout === 'function') socket.setTimeout(0);
  handle.onread = null;
  try { handle.close(); } catch { /* already gone */ }
}

// A duplicate made for a crossing that never happened is closed here rather
// than leaked.
function closeSent(carried) {
  try { netNative().closeFd(carried.fd); } catch { /* already gone */ }
}

// The text and descriptor one message crosses as. A sent connection is
// detached here rather than on the acknowledgement: the duplicate already
// holds the socket open, so this side has nothing left to keep.
function prepareSend(message, sendHandle, options) {
  const carried = describeHandle(sendHandle, options);
  let text;
  try {
    text = JSON.stringify(carried === null ? message : envelope(message, carried));
  } catch (error) {
    if (carried !== null) closeSent(carried);
    throw error;
  }
  if (text === undefined) {
    if (carried !== null) closeSent(carried);
    return undefined;
  }
  if (carried !== null) detachSent(carried);
  return { text, fd: carried === null ? -1 : carried.fd };
}

// The other end: what arrived becomes the kind the sender named.
function unwrap(message, fd) {
  const wrapped = message !== null && typeof message === 'object' &&
    message.cmd === HANDLE_ENVELOPE;
  if (!wrapped) {
    if (typeof fd === 'number' && fd >= 0) {
      try { netNative().closeFd(fd); } catch { /* already gone */ }
    }
    return { event: isInternal(message) ? 'internalMessage' : 'message', message };
  }
  const inner = message.msg;
  const event = isInternal(inner) ? 'internalMessage' : 'message';
  return { event, message: inner, handle: adoptHandle(fd, message.type, message.dgramType) };
}

function adoptHandle(fd, type, dgramType) {
  if (typeof fd !== 'number' || fd < 0) return undefined;
  // The descriptor says what it is; the kernel is the one that knows.
  const kind = netNative().socketKind(fd);
  if (kind === 2) return adoptDatagram(fd, type, dgramType);
  if (kind === 3) return adoptListener(fd, type);
  const id = netNative().adoptFd(fd);
  if (id < 0) return undefined;
  const { TCP } = require('internal/otter/tcp_wrap');
  const handle = TCP.adopt(id);
  if (type === 'net.Native') return handle;
  const net = require('net');
  return new net.Socket({ handle, readable: true, writable: true });
}

function adoptDatagram(fd, type, dgramType) {
  const id = require('internal/otter/dgram').adoptFd(fd);
  if (id < 0) return undefined;
  const { UDP } = require('internal/otter/udp_wrap');
  const handle = UDP.adopt(id);
  if (type !== 'dgram.Socket') return handle;
  const dgram = require('dgram');
  const socket = new dgram.Socket(dgramType ?? 'udp4');
  socket.bind({ fd: handle });
  return socket;
}

function adoptListener(fd, type) {
  const id = netNative().adoptListenerFd(fd);
  if (id < 0) return undefined;
  const { TCP } = require('internal/otter/tcp_wrap');
  const handle = TCP.adoptListener(id);
  if (type !== 'net.Server') return handle;
  const net = require('net');
  const server = new net.Server();
  server.listen(handle);
  return server;
}

// `dgram` keeps its handle behind a private symbol rather than on `_handle`,
// so the module that owns that symbol is the one asked for it.
function dgramHandleOf(value) {
  const { kStateSymbol } = require('internal/dgram');
  return value?.[kStateSymbol]?.handle;
}

function netNative() {
  return require('internal/otter/net');
}

function spawn(file, args, options) {
  const normalized = normalizeSpawnArguments(file, args, options);
  validateTimeout(normalized.timeout);
  validateAbortSignal(normalized.signal, 'options.signal');
  const killSignal = sanitizeKillSignal(normalized.killSignal);
  const { streams, wantsChannel } = normalizeStdio(normalized.stdio, ['pipe', 'pipe', 'pipe']);
  const cp = new ChildProcess();
  cp._run(normalized.file, normalized.args, { ...normalized, stdio: streams, ipc: wantsChannel });
  if (wantsChannel && cp._handle !== 0) {
    cp.connected = true;
    cp.channel = { ref() {}, unref() {} };
  }

  // A child that outlives its welcome is stopped, and the timer that would
  // stop it is dropped the moment it leaves on its own.
  if (normalized.timeout > 0) {
    let timer = setTimeout(() => {
      timer = null;
      try {
        cp.kill(killSignal);
      } catch (error) {
        cp.emit('error', error);
      }
    }, normalized.timeout);
    cp.once('exit', () => {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    });
  }

  const signal = normalized.signal;
  if (signal !== undefined && signal !== null) {
    const onAbort = () => {
      try {
        if (cp.kill(killSignal)) cp.emit('error', abortError(signal.reason));
      } catch (error) {
        cp.emit('error', error);
      }
    };
    if (signal.aborted) {
      process.nextTick(onAbort);
    } else {
      signal.addEventListener('abort', onAbort, { once: true });
      cp.once('exit', () => signal.removeEventListener('abort', onAbort));
    }
  }
  return cp;
}

// A stream read as text joins as text; one read as bytes concatenates.
function joinCollected(chunks, encoding, stream) {
  if (encoding || stream?.readableEncoding) return chunks.join('');
  return Buffer.concat(chunks);
}

function execFile(file, args, options, callback) {
  const normalized = normalizeExecFileArgs(file, args, options, callback);
  file = normalized.file;
  args = normalized.args;
  callback = normalized.callback;
  const settings = {
    encoding: 'utf8',
    timeout: 0,
    maxBuffer: MAX_BUFFER,
    killSignal: 'SIGTERM',
    shell: false,
    ...normalized.options,
  };
  validateTimeout(settings.timeout);
  validateMaxBuffer(settings.maxBuffer);
  settings.killSignal = sanitizeKillSignal(settings.killSignal);

  const child = spawn(file, args, {
    cwd: settings.cwd,
    env: settings.env,
    gid: settings.gid,
    uid: settings.uid,
    shell: settings.shell,
    signal: settings.signal,
    windowsHide: settings.windowsHide,
    windowsVerbatimArguments: settings.windowsVerbatimArguments,
  });

  const encoding = settings.encoding !== 'buffer' && Buffer.isEncoding(settings.encoding)
    ? settings.encoding
    : null;
  const collected = { stdout: [], stderr: [] };
  const kept = { stdout: 0, stderr: 0 };
  let failure = null;
  let exited = false;
  let timer = null;
  let command = file;

  function stop() {
    child.stdout?.destroy();
    child.stderr?.destroy();
    try {
      child.kill(settings.killSignal);
    } catch (error) {
      failure = error;
      finish();
    }
  }

  function finish(status, signal) {
    if (exited) return;
    exited = true;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    if (!callback) return;
    const stdout = joinCollected(collected.stdout, encoding, child.stdout);
    const stderr = joinCollected(collected.stderr, encoding, child.stderr);
    if (!failure && status === 0 && signal === null) {
      callback(null, stdout, stderr);
      return;
    }
    if (args?.length) command += ` ${args.join(' ')}`;
    if (!failure) {
      // A negative status is not an exit code but a platform failure, and it
      // reports itself by name the way every other one does.
      failure = outcomeError(`Command failed: ${command}\n${stderr}`, {
        code: status < 0 ? require('util').getSystemErrorName(status) : status,
        killed: child.killed,
        signal,
      });
    }
    failure.cmd = command;
    callback(failure, stdout, stderr);
  }

  // A stream is collected only as far as the caller agreed to hold it: past
  // that the run has failed, and the child is stopped rather than left
  // filling a buffer nobody will read.
  function watch(name, stream) {
    if (!stream) return;
    if (encoding) stream.setEncoding(encoding);
    stream.on('data', (chunk) => {
      const chunkEncoding = stream.readableEncoding;
      const length = chunkEncoding ? Buffer.byteLength(chunk, chunkEncoding) : chunk.length;
      kept[name] += length;
      if (kept[name] > settings.maxBuffer) {
        const room = settings.maxBuffer - (kept[name] - length);
        collected[name].push(chunk.slice(0, room));
        failure = new RangeError(`${name} maxBuffer length exceeded`);
        failure.code = 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER';
        stop();
        return;
      }
      collected[name].push(chunk);
    });
  }
  watch('stdout', child.stdout);
  watch('stderr', child.stderr);

  if (settings.timeout > 0) {
    timer = setTimeout(() => {
      timer = null;
      stop();
    }, settings.timeout);
  }

  child.addListener('close', finish);
  child.addListener('error', (error) => {
    failure = error;
    child.stdout?.destroy();
    child.stderr?.destroy();
    finish();
  });
  return child;
}

function exec(command, options, callback) {
  const normalized = normalizeExecArgs(command, options, callback);
  return execFile(normalized.file, normalized.options, normalized.callback);
}

// `util.promisify` on either of these answers `{ stdout, stderr }`, and a
// failure carries the same two on the error it rejects with.
function promisifiedRun(run) {
  return function promisified(...args) {
    let resolve;
    let reject;
    const promise = new Promise((ok, fail) => { resolve = ok; reject = fail; });
    promise.child = run(...args, (error, stdout, stderr) => {
      if (error !== null && error !== undefined) {
        error.stdout = stdout;
        error.stderr = stderr;
        reject(error);
      } else {
        resolve({ stdout, stderr });
      }
    });
    return promise;
  };
}

for (const [run, target] of [[exec, exec], [execFile, execFile]]) {
  Object.defineProperty(target, require('util').promisify.custom, {
    value: promisifiedRun(run),
    enumerable: false,
    writable: false,
    configurable: true,
  });
}

// What a one-word `stdio` stands for, plus the channel a fork always has.
function stdioStringToArray(stdio, channel) {
  let streams;
  switch (stdio) {
    case 'ignore':
    case 'overlapped':
    case 'pipe':
      streams = [stdio, stdio, stdio];
      break;
    case 'inherit':
      streams = ['inherit', 'inherit', 'inherit'];
      break;
    default:
      throw invalidArgValue('stdio', stdio, 'is invalid');
  }
  if (channel) streams.push(channel);
  return streams;
}

// A forked child runs this same binary and joins a channel opened for it, so
// `child.send` here and `process.send` there are the two ends of one channel.
function fork(modulePath, args, options) {
  nullByteCheck(modulePath, 'modulePath');
  if (Array.isArray(args)) {
    args = args.slice(0);
  } else if (args === undefined || args === null) {
    args = [];
  } else if (typeof args !== 'object') {
    throw invalidArgValue('args', args, 'is invalid');
  } else {
    options = args;
    args = [];
  }
  if (options === undefined || options === null) options = {};
  else if (typeof options !== 'object') throw invalidArgValue('options', options, 'is invalid');
  else options = { ...options };

  // A fork is this binary running a module, not a command line for a shell.
  options.shell = false;
  options.execPath = options.execPath || process.execPath;
  nullByteCheck(options.execPath, 'options.execPath', true);
  const execArgv = options.execArgv || process.execArgv || [];
  nullByteCheckAll(execArgv, 'options.execArgv');

  args = [...execArgv, String(modulePath), ...args];

  if (typeof options.stdio === 'string') {
    options.stdio = stdioStringToArray(options.stdio, 'ipc');
  } else if (!Array.isArray(options.stdio)) {
    options.stdio = stdioStringToArray(options.silent ? 'pipe' : 'inherit', 'ipc');
  } else if (!options.stdio.includes('ipc')) {
    const err = new Error('Forked processes must have an IPC channel, ' +
                          'missing value \'ipc\' in options.stdio');
    err.code = 'ERR_CHILD_PROCESS_IPC_REQUIRED';
    throw err;
  }

  return spawn(options.execPath, args, options);
}

module.exports = {
  spawn, spawnSync, exec, execSync, execFile, execFileSync, fork, ChildProcess,
};

// Host-dispatch hooks stay off the enumerable global surface: Node's
// test harness treats any enumerable global it does not know as a leak.
Object.defineProperty(globalThis, '__otterChildExit', { enumerable: false });
Object.defineProperty(globalThis, '__otterChildStdio', { enumerable: false });
Object.defineProperty(globalThis, '__otterChildIpc', { enumerable: false });

// The other end of the same crossing: a descriptor that arrives on this
// process's own channel becomes the socket its `message` listener is handed.
// The channel a process is launched with belongs to how it was started, not
// to this module, so the host owns `process.send`. The two halves of the
// handle protocol live here, where sockets live, and the host reaches them
// through these hooks.
globalThis.__otterIpcPrepareSend = prepareSend;
globalThis.__otterIpcDeliver = unwrap;
for (const name of ['__otterIpcPrepareSend', '__otterIpcDeliver']) {
  Object.defineProperty(globalThis, name, { enumerable: false });
}
