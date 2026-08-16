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

function toLatin1(data, encoding) {
  if (typeof data === 'string') return Buffer.from(data, encoding || 'utf8').toString('latin1');
  if (Buffer.isBuffer(data)) return data.toString('latin1');
  if (ArrayBuffer.isView(data)) {
    return Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString('latin1');
  }
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('latin1');
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

function Socket(options) {
  if (!(this instanceof Socket)) return new Socket(options);
  options = options || {};
  Duplex.call(this, options);
  this._handle = 0;
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

// The native half pushes; nothing is pulled, so the read side only has to
// exist.
Socket.prototype._read = function _read() {};

Socket.prototype._write = function _write(chunk, encoding, callback) {
  if (this._handle === 0) {
    callback(codedError('This socket is closed', 'ERR_SOCKET_CLOSED'));
    return;
  }
  const payload = toLatin1(chunk, encoding);
  this.bytesWritten += payload.length;
  native.write(this._handle, payload);
  this._touch();
  callback();
};

Socket.prototype._final = function _final(callback) {
  if (this._handle !== 0) native.end(this._handle);
  this._writeEnded = true;
  callback();
  this._maybeDestroy();
};

Socket.prototype._destroy = function _destroy(error, callback) {
  this.destroyed = true;
  if (this._handle !== 0) {
    connections.delete(this._handle);
    native.close(this._handle);
    this._handle = 0;
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
  if (this._handle !== 0) native.setOption(this._handle, 'setNoDelay', enable !== false);
  return this;
};

Socket.prototype.setKeepAlive = function setKeepAlive() {
  return this;
};

Socket.prototype.address = function address() {
  if (this._handle === 0) return {};
  return native.address(this._handle, 'local') ?? {};
};

Socket.prototype.ref = function ref() {
  if (this._handle !== 0) native.hold(this._handle, true);
  return this;
};

Socket.prototype.unref = function unref() {
  if (this._handle !== 0) native.hold(this._handle, false);
  return this;
};

Socket.prototype.connect = function connect(...args) {
  const { port, host, callback } = normalizeConnectArgs(args);
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
  this._handle = handle;
  this.connecting = false;
  this.pending = false;
  connections.set(handle, this);
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

module.exports = {
  Server,
  Socket,
  Stream: Socket,
  createServer,
  createConnection: connect,
  connect,
  isIP,
  isIPv4,
  isIPv6,
  getDefaultAutoSelectFamily: () => false,
  setDefaultAutoSelectFamily: () => {},
  getDefaultAutoSelectFamilyAttemptTimeout: () => 250,
  setDefaultAutoSelectFamilyAttemptTimeout: () => {},
};
