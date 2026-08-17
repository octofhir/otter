'use strict';
// `node:net` — TCP servers and connections.
//
// The native half owns the sockets and the loops that drive them; this side
// owns the event surface and the handle tables the loops dispatch through.
// A connection is a Duplex, so it pipes and streams like any other.

const EventEmitter = require('events');
const { Buffer } = require('buffer');
const { Duplex } = require('stream');
const native = globalThis.__otterNetNative;

const servers = new Map();
const connections = new Map();
const pending = new Map();
let nextToken = 1;

// The wire payload stays a byte view all the way into the native write;
// only genuine strings are encoded first.
function toWireBuffer(data, encoding) {
  if (Buffer.isBuffer(data)) return data;
  if (typeof data === 'string') return Buffer.from(data, encoding || 'utf8');
  if (ArrayBuffer.isView(data)) {
    return Buffer.from(data.buffer, data.byteOffset, data.byteLength);
  }
  if (data instanceof ArrayBuffer) return Buffer.from(data);
  throw invalidArgType('data', 'string or an instance of Buffer, TypedArray, or DataView', data);
}

function invalidArgType(name, expected, value) {
  const received = value === null || value === undefined
    ? ` Received ${value}`
    : typeof value === 'string'
      ? ` Received type string ('${value}')`
      : ` Received type ${typeof value} (${String(value)})`;
  const err = new TypeError(`The "${name}" argument must be of type ${expected}.${received}`);
  err.code = 'ERR_INVALID_ARG_TYPE';
  return err;
}

function codedError(message, code) {
  const err = new Error(message);
  err.code = code;
  return err;
}

// The vendored http stack touches `socket._handle` as an object — read
// flow flags, an `onread` hook slot, and async-id plumbing. The numeric
// native id lives behind it as `_fd`; `isStreamBase` stays false so the
// parser-consume fast path is never taken and data flows as JS 'data'.
function SocketHandle(socket, fd) {
  this._socket = socket;
  this._fd = fd;
  this.reading = true;
  this.onread = null;
  this.isStreamBase = false;
  this._consumed = false;
}
SocketHandle.prototype.readStart = function readStart() { this.reading = true; return 0; };
SocketHandle.prototype.readStop = function readStop() { this.reading = false; return 0; };
SocketHandle.prototype.getAsyncId = function getAsyncId() { return -1; };

function Socket(options) {
  if (!(this instanceof Socket)) return new Socket(options);
  options = options || {};
  Duplex.call(this, options);
  this._handle = null;
  this.connecting = false;
  this.destroyed = false;
  this.pending = true;
  this.bytesRead = 0;
  this.bytesWritten = 0;
  this.remoteAddress = undefined;
  this.remotePort = undefined;
  this.remoteFamily = undefined;
  this.localAddress = undefined;
  this.localPort = undefined;
  this._timeout = 0;
  this._timer = null;
  // Half-open is opt-in, as it is in Node: by default a socket whose peer
  // has finished speaking finishes too, and is done once both directions
  // are.
  this.allowHalfOpen = options.allowHalfOpen === true;
  this._readEnded = false;
  this._writeEnded = false;
}
Object.setPrototypeOf(Socket.prototype, Duplex.prototype);
Object.setPrototypeOf(Socket, Duplex);

Socket.prototype._maybeDestroy = function _maybeDestroy() {
  if (this._readEnded && this._writeEnded && !this.destroyed) this.destroy();
};

// Close once pending writes have flushed — writes here reach the native
// layer synchronously, so this is end() plus a destroy on 'finish'.
Socket.prototype.destroySoon = function destroySoon() {
  if (this.writableFinished) {
    this.destroy();
  } else {
    this.once('finish', () => this.destroy());
    this.end();
  }
};

Socket.prototype._unrefTimer = function _unrefTimer() {
  this._touch();
};

// The native half pushes; nothing is pulled, so the read side only has to
// exist.
Socket.prototype._read = function _read() {};

Socket.prototype._write = function _write(chunk, encoding, callback) {
  if (this.connecting) {
    // Node's net.Socket accepts writes before the connection exists and
    // flushes them on connect, in order.
    (this._pendingWrites ??= []).push([chunk, encoding]);
    callback();
    return;
  }
  if (this._handle === null) {
    callback(codedError('This socket is closed', 'ERR_SOCKET_CLOSED'));
    return;
  }
  const payload = toWireBuffer(chunk, encoding);
  this.bytesWritten += payload.length;
  native.write(this._handle._fd, payload);
  this._touch();
  callback();
};

// Corked writes (the vendored http stack corks around header+body flushes)
// leave in one native write, so raw peers see them as one packet.
Socket.prototype._writev = function _writev(chunks, callback) {
  if (this.connecting) {
    (this._pendingWrites ??= []).push(...chunks.map((c) => [c.chunk, c.encoding]));
    callback();
    return;
  }
  if (this._handle === null) {
    callback(codedError('This socket is closed', 'ERR_SOCKET_CLOSED'));
    return;
  }
  const payload = Buffer.concat(chunks.map(({ chunk, encoding }) => toWireBuffer(chunk, encoding)));
  this.bytesWritten += payload.length;
  native.write(this._handle._fd, payload);
  this._touch();
  callback();
};

Socket.prototype._final = function _final(callback) {
  if (this.connecting) {
    this._pendingFinal = true;
    callback();
    return;
  }
  if (this._handle !== null) native.end(this._handle._fd);
  this._writeEnded = true;
  callback();
  this._maybeDestroy();
};

Socket.prototype._destroy = function _destroy(error, callback) {
  this.destroyed = true;
  if (this._handle !== null) {
    connections.delete(this._handle._fd);
    native.close(this._handle._fd);
    this._handle = null;
  }
  this._clearTimer();
  callback(error);
};

// `setTimeout` here is inactivity on the connection, not a plain timer: any
// traffic in either direction restarts it.
Socket.prototype.setTimeout = function setTimeout(timeout, callback) {
  this._timeout = timeout;
  if (typeof callback === 'function') {
    if (timeout === 0) this.removeListener('timeout', callback);
    else this.once('timeout', callback);
  }
  this._touch();
  return this;
};

Socket.prototype._touch = function _touch() {
  this._clearTimer();
  if (this._timeout > 0 && !this.destroyed) {
    this._timer = setTimeout(() => this.emit('timeout'), this._timeout);
  }
};

Socket.prototype._clearTimer = function _clearTimer() {
  if (this._timer !== null) {
    clearTimeout(this._timer);
    this._timer = null;
  }
};

Socket.prototype.setNoDelay = function setNoDelay(enable = true) {
  if (this._handle !== null) native.setOption(this._handle._fd, 'setNoDelay', enable !== false);
  return this;
};

Socket.prototype.setKeepAlive = function setKeepAlive() {
  return this;
};

Socket.prototype.address = function address() {
  if (this._handle === null) return {};
  return native.address(this._handle._fd, 'local') ?? {};
};

Socket.prototype.ref = function ref() {
  if (this._handle !== null) native.hold(this._handle._fd, true);
  return this;
};

Socket.prototype.unref = function unref() {
  if (this._handle !== null) native.hold(this._handle._fd, false);
  return this;
};

Socket.prototype.connect = function connect(...args) {
  const { port, host, callback } = normalizeConnectArgs(args);
  const options = args[0] !== null && typeof args[0] === 'object' ? args[0] : null;
  if (options && typeof options.timeout === 'number' && options.timeout > 0) {
    this.setTimeout(options.timeout);
  }
  if (typeof callback === 'function') this.once('connect', callback);
  this.connecting = true;
  const token = nextToken++;
  pending.set(token, this);
  try {
    native.connect(host, port, token);
  } catch (error) {
    pending.delete(token);
    this.connecting = false;
    setTimeout(() => this.destroy(error), 0);
  }
  return this;
};

// The native half hands over an established connection, from either side.
Socket.prototype._adopt = function _adopt(handle, remote) {
  if (this.destroyed) {
    // Destroyed while the connect was in flight: the handle arrives with
    // nobody to own it, so it closes instead of leaking a live loop.
    native.close(handle);
    return;
  }
  this._handle = new SocketHandle(this, handle);
  this.connecting = false;
  this.pending = false;
  connections.set(handle, this);
  if (this._pendingWrites !== undefined) {
    const queued = this._pendingWrites;
    this._pendingWrites = undefined;
    for (const [chunk, encoding] of queued) {
      const payload = toWireBuffer(chunk, encoding);
      this.bytesWritten += payload.length;
      native.write(handle, payload);
    }
  }
  if (this._pendingFinal === true) {
    this._pendingFinal = false;
    native.end(handle);
    this._writeEnded = true;
  }
  if (remote) {
    this.remoteAddress = remote.address;
    this.remotePort = remote.port;
    this.remoteFamily = remote.family;
  }
  const local = native.address(handle, 'local');
  if (local) {
    this.localAddress = local.address;
    this.localPort = local.port;
  }
  this._touch();
};

Socket.prototype._received = function _received(payload) {
  const chunk = Buffer.from(payload, 'latin1');
  this.bytesRead += chunk.length;
  this._touch();
  this.push(chunk);
};

Socket.prototype._ended = function _ended() {
  this._clearTimer();
  this._readEnded = true;
  this.push(null);
  if (!this.allowHalfOpen) this.end();
  this._maybeDestroy();
};

// Node's own arg canonicalization: `[options, callback]`, marked so a
// double normalize is a no-op. The vendored http agent calls this directly.
const normalizedArgsSymbol = Symbol('normalizedArgs');
function _normalizeArgs(args) {
  let arr;
  if (args.length === 0) {
    arr = [{}, null];
  } else if (args[0] !== null && typeof args[0] === 'object') {
    arr = [args[0], typeof args[1] === 'function' ? args[1] : null];
  } else {
    const options = { port: args[0] };
    let callback = null;
    if (typeof args[1] === 'string') {
      options.host = args[1];
      if (typeof args[2] === 'function') callback = args[2];
    } else if (typeof args[1] === 'function') {
      callback = args[1];
    }
    arr = [options, callback];
  }
  arr[normalizedArgsSymbol] = true;
  return arr;
}

function normalizeConnectArgs(args) {
  let port;
  let host;
  let callback;
  const first = args[0];
  if (first !== null && typeof first === 'object') {
    port = first.port;
    host = first.host;
  } else {
    port = first;
    if (typeof args[1] === 'string') host = args[1];
  }
  for (const argument of args) {
    if (typeof argument === 'function') callback = argument;
  }
  return { port: Number(port) || 0, host: host ?? 'localhost', callback };
}

function Server(options, listener) {
  if (!(this instanceof Server)) return new Server(options, listener);
  EventEmitter.call(this);
  if (typeof options === 'function') { listener = options; options = {}; }
  this._handle = 0;
  this._connections = 0;
  this.listening = false;
  this._options = options ?? {};
  if (typeof listener === 'function') this.on('connection', listener);
}
Object.setPrototypeOf(Server.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Server, EventEmitter);

Server.prototype.listen = function listen(...args) {
  let port = 0;
  let host;
  let callback;
  const first = args[0];
  if (first !== null && typeof first === 'object') {
    port = first.port ?? 0;
    host = first.host;
  } else if (typeof first === 'number' || typeof first === 'string') {
    port = Number(first) || 0;
    if (typeof args[1] === 'string') host = args[1];
  }
  for (const argument of args) {
    if (typeof argument === 'function') callback = argument;
  }
  if (typeof callback === 'function') this.once('listening', callback);

  let bound;
  try {
    bound = native.listen(host ?? '', port);
  } catch (error) {
    setTimeout(() => this.emit('error', error), 0);
    return this;
  }
  this._handle = bound.handle;
  this._address = bound.address;
  this.listening = true;
  servers.set(bound.handle, this);
  setTimeout(() => this.emit('listening'), 0);
  return this;
};

Server.prototype.address = function address() {
  return this.listening ? this._address : null;
};

Server.prototype.getConnections = function getConnections(callback) {
  if (typeof callback === 'function') {
    setTimeout(() => callback(null, this._connections), 0);
  }
  return this;
};

Server.prototype.close = function close(callback) {
  if (!this.listening) {
    const err = codedError('Server is not running.', 'ERR_SERVER_NOT_RUNNING');
    if (typeof callback === 'function') setTimeout(() => callback(err), 0);
    return this;
  }
  if (typeof callback === 'function') this.once('close', callback);
  this.listening = false;
  servers.delete(this._handle);
  native.close(this._handle);
  this._handle = 0;
  // Node reports the close once the connections it accepted are gone; with
  // none outstanding that is the next turn.
  setTimeout(() => this.emit('close'), 0);
  return this;
};

Server.prototype.ref = function ref() {
  if (this._handle !== 0) native.hold(this._handle, true);
  return this;
};

Server.prototype.unref = function unref() {
  if (this._handle !== 0) native.hold(this._handle, false);
  return this;
};

Server.prototype._accepted = function _accepted(handle, remote) {
  const socket = new Socket();
  socket._adopt(handle, remote);
  socket.server = this;
  this._connections += 1;
  socket.once('close', () => { this._connections -= 1; });
  this.emit('connection', socket);
};

// The native loops dispatch here, on the isolate thread.
globalThis.__otterNetDeliver = function deliver(kind, first, second, third) {
  if (kind === 'accept') {
    const server = servers.get(first);
    if (server === undefined) return;
    server._accepted(second, third);
    return;
  }
  if (kind === 'connect') {
    const socket = pending.get(first);
    pending.delete(first);
    if (socket === undefined) return;
    socket._adopt(second, undefined);
    socket.emit('connect');
    socket.emit('ready');
    return;
  }
  if (kind === 'connectError') {
    const socket = pending.get(first);
    pending.delete(first);
    if (socket === undefined) return;
    socket.connecting = false;
    socket.destroy(codedError(third, second));
    return;
  }
  const socket = connections.get(first);
  if (socket === undefined) return;
  if (kind === 'data') socket._received(second);
  else if (kind === 'end') socket._ended();
};

function createServer(options, listener) {
  return new Server(options, listener);
}

function connect(...args) {
  const socket = new Socket();
  return socket.connect(...args);
}

const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

function isIPv4(input) {
  const parts = IPV4.exec(String(input));
  if (parts === null) return false;
  return parts.slice(1).every((part) => part.length === 1 || part[0] !== '0')
    && parts.slice(1).every((part) => Number(part) <= 255);
}

function isIPv6(input) {
  const text = String(input);
  if (!text.includes(':')) return false;
  // A compressed run may appear once, and every group is up to four hex digits.
  const compressed = text.split('::');
  if (compressed.length > 2) return false;
  const groups = text.replace('::', ':').split(':').filter((group) => group !== '');
  return groups.every((group) => /^[0-9a-fA-F]{1,4}$/.test(group) || isIPv4(group));
}

function isIP(input) {
  if (isIPv4(input)) return 4;
  if (isIPv6(input)) return 6;
  return 0;
}


// §https://nodejs.org/api/net.html#class-netblocklist — address deny rules.
// v4 addresses map to a 32-bit integer, v6 to a 128-bit BigInt; a v4 rule
// also matches its ::ffff: v6 mapping the way Node's does.
function parseV4(addr) {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(String(addr));
  if (m === null) return null;
  let out = 0;
  for (let i = 1; i <= 4; i++) {
    const part = Number(m[i]);
    if (part > 255) return null;
    out = out * 256 + part;
  }
  return out;
}

function parseV6(addr) {
  let text = String(addr).toLowerCase();
  if (text.startsWith('[') && text.endsWith(']')) text = text.slice(1, -1);
  const zone = text.indexOf('%');
  if (zone !== -1) text = text.slice(0, zone);
  let head = text;
  let tail = '';
  const gap = text.indexOf('::');
  if (gap !== -1) {
    head = text.slice(0, gap);
    tail = text.slice(gap + 2);
    if (tail.includes('::')) return null;
  }
  const expand = (part) => (part === '' ? [] : part.split(':'));
  const headParts = expand(head);
  const tailParts = expand(tail);
  const last = tailParts.length > 0 ? tailParts[tailParts.length - 1]
    : headParts[headParts.length - 1];
  if (last !== undefined && last.includes('.')) {
    const v4 = parseV4(last);
    if (v4 === null) return null;
    const words = [(v4 >>> 16).toString(16), (v4 & 0xffff).toString(16)];
    if (tailParts.length > 0) tailParts.splice(-1, 1, ...words);
    else headParts.splice(-1, 1, ...words);
  }
  const missing = 8 - headParts.length - tailParts.length;
  if (gap === -1 ? missing !== 0 : missing < 0) return null;
  const words = [...headParts, ...new Array(gap === -1 ? 0 : missing).fill('0'), ...tailParts];
  if (words.length !== 8) return null;
  let out = 0n;
  for (const word of words) {
    if (!/^[0-9a-f]{1,4}$/.test(word)) return null;
    out = (out << 16n) | BigInt(parseInt(word, 16));
  }
  return out;
}

const V4_MAPPED_PREFIX = 0xffffn << 32n;

function blockKey(addr, family) {
  if (String(family).toLowerCase() === 'ipv6') {
    const v6 = parseV6(addr);
    if (v6 === null) return null;
    // ::ffff:a.b.c.d compares against v4 rules too.
    if ((v6 >> 32n) === 0xffffn) return { family: 'ipv4', value: Number(v6 & 0xffffffffn) };
    return { family: 'ipv6', value: v6 };
  }
  const v4 = parseV4(addr);
  if (v4 === null) return null;
  return { family: 'ipv4', value: v4 };
}

class BlockList {
  #rules = [];

  addAddress(address, family = 'ipv4') {
    const key = blockKey(address, family);
    if (key === null) throw invalidArgType('address', 'a valid IP address', address);
    this.#rules.push({ kind: 'Address', family: key.family, start: key.value, end: key.value, text: `Address: ${key.family.toUpperCase()} ${address}` });
  }

  addRange(start, end, family = 'ipv4') {
    const from = blockKey(start, family);
    const to = blockKey(end, family);
    if (from === null || to === null || from.family !== to.family) {
      throw invalidArgType('start', 'a valid IP range', start);
    }
    this.#rules.push({ kind: 'Range', family: from.family, start: from.value, end: to.value, text: `Range: ${from.family.toUpperCase()} ${start}-${end}` });
  }

  addSubnet(network, prefix, family = 'ipv4') {
    const key = blockKey(network, family);
    if (key === null) throw invalidArgType('network', 'a valid IP address', network);
    if (key.family === 'ipv4') {
      const bits = 32 - prefix;
      const start = bits >= 32 ? 0 : (key.value >>> 0) & (bits === 0 ? 0xffffffff : (~0 << bits) >>> 0);
      const end = bits === 0 ? start : (start + 2 ** bits - 1) >>> 0;
      this.#rules.push({ kind: 'Subnet', family: 'ipv4', start, end, text: `Subnet: IPV4 ${network}/${prefix}` });
    } else {
      const bits = BigInt(128 - prefix);
      const mask = bits === 0n ? (1n << 128n) - 1n : ((1n << 128n) - 1n) ^ ((1n << bits) - 1n);
      const start = key.value & mask;
      const end = start + (1n << bits) - 1n;
      this.#rules.push({ kind: 'Subnet', family: 'ipv6', start, end, text: `Subnet: IPV6 ${network}/${prefix}` });
    }
  }

  check(address, family = 'ipv4') {
    const key = blockKey(address, family);
    if (key === null) return false;
    for (const rule of this.#rules) {
      if (rule.family !== key.family) continue;
      if (key.value >= rule.start && key.value <= rule.end) return true;
    }
    return false;
  }

  get rules() {
    return this.#rules.map((rule) => rule.text);
  }

  static isBlockList(value) {
    return value instanceof BlockList;
  }
}

module.exports = {
  BlockList,
  Server,
  Socket,
  Stream: Socket,
  createServer,
  createConnection: connect,
  connect,
  _normalizeArgs,
  isIP,
  isIPv4,
  isIPv6,
  getDefaultAutoSelectFamily: () => false,
  setDefaultAutoSelectFamily: () => {},
  getDefaultAutoSelectFamilyAttemptTimeout: () => 250,
  setDefaultAutoSelectFamilyAttemptTimeout: () => {},
};
