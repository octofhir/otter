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
    env: options.env === undefined || options.env === null
      ? { ...process.env }
      : options.env,
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

function spawn(command, args, options) {
  const n = normalizeArgs(command, args, options);
  const { streams, wantsChannel } = normalizeStdio(n.options.stdio, ['pipe', 'pipe', 'pipe']);
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
  // `args` is optional: `fork(path, options)` puts the settings second.
  const a = Array.isArray(args) ? args : [];
  const given = (Array.isArray(args) ? options : args) || options || {};
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
