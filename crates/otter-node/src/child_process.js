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
  kill(signal) { this.killed = true; this.emit('exit', null, signal || 'SIGTERM'); return true; }
  ref() {}
  unref() {}
  disconnect() { this.connected = false; }
  _run(command, args, options) {
    setTimeout(() => {
      const raw = rawSpawn(command, args, options);
      this.pid = raw.pid;
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
