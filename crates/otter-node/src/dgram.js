'use strict';
// `node:dgram` — UDP sockets.
//
// The native half owns the socket and the receive loop; this side owns the
// event surface and the handle table the loop dispatches through.

const EventEmitter = require('events');
const { Buffer } = require('buffer');
const native = globalThis.__otterDgramNative;

const sockets = new Map();

function toLatin1(data, encoding) {
  if (typeof data === 'string') return Buffer.from(data, encoding || 'utf8').toString('latin1');
  if (Array.isArray(data)) return data.map((part) => toLatin1(part, encoding)).join('');
  if (Buffer.isBuffer(data)) return data.toString('latin1');
  if (ArrayBuffer.isView(data)) {
    return Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString('latin1');
  }
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('latin1');
  throw invalidArgType('buffer', 'string or an instance of Buffer, TypedArray, or DataView', data);
}

function socketError(message, code) {
  const err = new Error(message);
  err.code = code;
  return err;
}

// The synchronous entry points do no name resolution, so they take a literal
// address or nothing at all.
function isNumericAddress(address) {
  if (address.includes(':')) return /^[0-9a-fA-F:.]+$/.test(address);
  const parts = address.split('.');
  return parts.length === 4 &&
    parts.every((part) => /^\d{1,3}$/.test(part) && Number(part) <= 255);
}

function requireNumericAddress(address, name) {
  if (typeof address !== 'string') throw invalidArgType(name, 'string', address);
  if (address !== '' && !isNumericAddress(address)) {
    const err = new TypeError(
      `The argument '${name}' must be a numeric address. Received ${JSON.stringify(address)}`);
    err.code = 'ERR_INVALID_ARG_VALUE';
    throw err;
  }
  return address;
}

function validatePort(port) {
  const value = Number(port);
  if (!Number.isInteger(value) || value <= 0 || value > 65535) {
    const err = new RangeError(
      `Port should be > 0 and < 65536. Received ${port}.`);
    err.code = 'ERR_SOCKET_BAD_PORT';
    throw err;
  }
  return value;
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

class Socket extends EventEmitter {
  #handle = 0;
  #type;
  #sendBlockList = null;
  #receiveBlockList = null;
  #bound = false;
  #closed = false;
  #remote = null;
  #connecting = false;
  #binding = false;
  #pendingSends = [];

  constructor(options = {}, listener) {
    super();
    const settings = typeof options === 'string' ? { type: options } : (options ?? {});
    if (settings.type !== 'udp4' && settings.type !== 'udp6') {
      const err = new TypeError(`Bad socket type specified. Valid types are: udp4, udp6`);
      err.code = 'ERR_SOCKET_BAD_TYPE';
      throw err;
    }
    this.#type = settings.type;
    if (typeof listener === 'function') this.on('message', listener);
    if (typeof settings.recvBufferSize === 'number') this._recvBufferSize = settings.recvBufferSize;
    this.#sendBlockList = settings.sendBlockList ?? null;
    this.#receiveBlockList = settings.receiveBlockList ?? null;
  }

  #blockFamily() {
    return this.#type === 'udp6' ? 'ipv6' : 'ipv4';
  }

  #blockedError(address) {
    const err = new Error(`IP address is not allowed: ${address}`);
    err.code = 'ERR_IP_BLOCKED';
    return err;
  }

  get type() { return this.#type; }

  #bindNow(port, address, callback) {
    if (typeof port === 'function') { callback = port; port = 0; address = undefined; }
    if (typeof address === 'function') { callback = address; address = undefined; }
    if (port !== null && typeof port === 'object') {
      address = port.address;
      port = port.port ?? 0;
    }
    if (this.#bound) {
      const err = new Error('Socket is already bound');
      err.code = 'ERR_SOCKET_ALREADY_BOUND';
      throw err;
    }

    let bound;
    try {
      bound = native.bind(this.#type, Number(port) || 0, address ?? '');
    } catch (error) {
      // Node's bind errors carry the address that failed.
      if (error !== null && typeof error === 'object' && typeof address === 'string') {
        error.address ??= address;
      }
      throw error;
    }
    this.#handle = bound.handle;
    this.#bound = true;
    sockets.set(bound.handle, this);
    if (typeof callback === 'function') this.once('listening', callback);
    setTimeout(() => {
      this.#binding = false;
      if (!this.#closed) this.emit('listening');
    }, 0);
    return bound;
  }

  bind(port, address, callback) {
    try {
      // An asynchronous bind is not finished until `listening`, and the
      // synchronous entry points refuse to run against a socket mid-bind.
      this.#binding = true;
      this.#bindNow(port, address, callback);
    } catch (error) {
      if (error.code === 'ERR_SOCKET_ALREADY_BOUND') throw error;
      // A failed bind is reported to the caller, not thrown at it: Node emits
      // it on the socket so an `error` listener sees it.
      setTimeout(() => this.emit('error', error), 0);
    }
    return this;
  }

  // `bindSync` is the same bind reported to the caller directly: the address
  // is already known by the time it returns, and `listening` still arrives on
  // the next tick.
  bindSync(options = {}) {
    if (options === null || typeof options !== 'object') {
      throw invalidArgType('options', 'Object', options);
    }
    const address = requireNumericAddress(options.address ?? '', 'address');
    const port = options.port ?? 0;
    if (!Number.isInteger(Number(port)) || Number(port) < 0 || Number(port) > 65535) {
      const err = new RangeError(`Port should be >= 0 and < 65536. Received ${port}.`);
      err.code = 'ERR_SOCKET_BAD_PORT';
      throw err;
    }
    const bound = this.#bindNow(Number(port), address);
    return { address: bound.address, family: bound.family, port: bound.port };
  }

  // `send(msg, [offset, length,] [port] [, address] [, callback])`. The offset
  // and length are present only when both are numbers and a port follows; on a
  // connected socket the port and address are the peer's and must be absent.
  send(msg, ...rest) {
    let offset = 0;
    let length;
    let port;
    let address;
    let callback;

    if (typeof rest[0] === 'number' && typeof rest[1] === 'number') {
      [offset, length] = rest;
      rest = rest.slice(2);
    }
    if (typeof rest[0] === 'number') {
      port = rest[0];
      rest = rest.slice(1);
    }
    for (const argument of rest) {
      if (typeof argument === 'function') callback = argument;
      else if (typeof argument === 'string') address = argument;
    }

    if (this.#remote !== null) {
      if (port !== undefined || address !== undefined) {
        throw socketError('Already connected', 'ERR_SOCKET_DGRAM_IS_CONNECTED');
      }
      port = this.#remote.port;
      address = this.#remote.address;
    } else if (port === undefined) {
      throw socketError('Not connected', 'ERR_SOCKET_DGRAM_NOT_CONNECTED');
    }

    let payload = toLatin1(msg);
    if (typeof length === 'number') payload = payload.slice(offset, offset + length);

    const target = address ?? (this.#type === 'udp6' ? '::1' : '127.0.0.1');
    if (this.#sendBlockList?.check(target, this.#blockFamily())) {
      const err = this.#blockedError(target);
      setTimeout(() => {
        if (typeof callback === 'function') return callback(err);
        this.emit('error', err);
      }, 0);
      return this;
    }
    if (!this.#bound) this.bind(0);
    // Delivery happens on the next tick, but close() drains the queue
    // first — a close() right after send() must not cancel the datagram.
    const entry = { payload, port: Number(port) || 0, address: address ?? '', target, callback, done: false };
    this.#pendingSends.push(entry);
    setTimeout(() => this.#deliverSend(entry), 0);
    return this;
  }

  #deliverSend(entry) {
    if (entry.done) return;
    entry.done = true;
    const index = this.#pendingSends.indexOf(entry);
    if (index !== -1) this.#pendingSends.splice(index, 1);
    let sent = 0;
    let sendError = null;
    try {
      sent = native.send(this.#handle, entry.payload, entry.port, entry.address);
    } catch (error) {
      // Node's send errors carry the destination.
      if (error !== null && typeof error === 'object') {
        error.address ??= entry.target;
        error.port ??= entry.port;
      }
      sendError = error;
    }
    if (this.#closed || entry.silent === true) return;
    if (sendError !== null) {
      if (typeof entry.callback === 'function') return entry.callback(sendError);
      return this.emit('error', sendError);
    }
    if (typeof entry.callback === 'function') entry.callback(null, sent);
  }

  // A connected socket records its peer and sends there; the connection is not
  // pushed down to the socket, so a datagram from elsewhere still arrives — a
  // difference from Node worth knowing before relying on peer filtering.
  connect(port, address, callback) {
    // Node validates the port before it looks at the connection state, so a
    // bad port is reported as a bad port even on a connected socket.
    validatePort(port);
    if (typeof address === 'function') { callback = address; address = undefined; }
    if (address !== undefined && address !== null && typeof address !== 'string') {
      throw invalidArgType('address', 'string', address);
    }
    if (this.#remote !== null || this.#connecting) {
      throw socketError('Already connected', 'ERR_SOCKET_DGRAM_IS_CONNECTED');
    }
    if (this.#sendBlockList?.check(address ?? '127.0.0.1', this.#blockFamily())) {
      const err = this.#blockedError(address ?? '127.0.0.1');
      setTimeout(() => {
        if (typeof callback === 'function') return callback(err);
        this.emit('error', err);
      }, 0);
      return;
    }
    this.#connecting = true;
    if (typeof callback === 'function') this.once('connect', callback);
    if (!this.#bound) this.bind(0);
    setTimeout(() => {
      let peer;
      try {
        peer = native.resolve(Number(port), address ?? '', this.#type);
      } catch (error) {
        this.#connecting = false;
        return this.emit('error', error);
      }
      this.#connecting = false;
      this.#remote = peer;
      if (!this.#closed) this.emit('connect');
    }, 0);
  }

  // `connectSync` resolves the peer before it returns, so `remoteAddress()` is
  // valid at once; `connect` still arrives on the next tick.
  connectSync(port, address) {
    validatePort(port);
    requireNumericAddress(address ?? '', 'address');
    if (this.#binding) {
      throw socketError('Socket is already bound', 'ERR_SOCKET_ALREADY_BOUND');
    }
    if (this.#remote !== null || this.#connecting) {
      throw socketError('Already connected', 'ERR_SOCKET_DGRAM_IS_CONNECTED');
    }
    if (this.#sendBlockList?.check(address ?? '127.0.0.1', this.#blockFamily())) {
      throw this.#blockedError(address ?? '127.0.0.1');
    }
    if (!this.#bound) this.#bindNow(0);
    this.#remote = native.resolve(Number(port), address ?? '', this.#type);
    setTimeout(() => { if (!this.#closed) this.emit('connect'); }, 0);
    return { ...this.#remote };
  }

  disconnect() {
    if (this.#remote === null) {
      throw socketError('Not connected', 'ERR_SOCKET_DGRAM_NOT_CONNECTED');
    }
    this.#remote = null;
  }

  remoteAddress() {
    if (this.#remote === null) {
      throw socketError('Not connected', 'ERR_SOCKET_DGRAM_NOT_CONNECTED');
    }
    return { ...this.#remote };
  }

  address() {
    if (!this.#bound) {
      // A closed socket reports not-running; one that was never bound has
      // no descriptor and fails the way the getsockname syscall would.
      if (this.#closed) {
        const err = new Error('Socket is not running');
        err.code = 'ERR_SOCKET_DGRAM_NOT_RUNNING';
        throw err;
      }
      const err = new Error('getsockname EBADF');
      err.code = 'EBADF';
      err.errno = -9;
      err.syscall = 'getsockname';
      throw err;
    }
    return native.address(this.#handle);
  }

  close(callback) {
    if (this.#closed) {
      const err = new Error('Not running');
      err.code = 'ERR_SOCKET_DGRAM_NOT_RUNNING';
      if (typeof callback === 'function') { callback(err); return this; }
      throw err;
    }
    // Queued datagrams leave before the descriptor goes away, silently:
    // Node never reports on a send whose socket closed before the tick.
    while (this.#pendingSends.length > 0) {
      const entry = this.#pendingSends[0];
      entry.silent = true;
      this.#deliverSend(entry);
    }
    this.#closed = true;
    if (typeof callback === 'function') this.once('close', callback);
    if (this.#bound) {
      sockets.delete(this.#handle);
      native.close(this.#handle);
      this.#bound = false;
    }
    setTimeout(() => this.emit('close'), 0);
    return this;
  }

  // Explicit resource management: disposing closes; a socket already closed
  // disposes as a no-op.
  [Symbol.asyncDispose]() {
    return new Promise((resolve) => {
      if (this.#closed) return resolve();
      this.close(() => resolve());
    });
  }

  // Sends complete synchronously in this realm, so the kernel-side queue the
  // native binding would report is always drained.
  getSendQueueSize() {
    return 0;
  }

  getSendQueueCount() {
    return 0;
  }

  setBroadcast(on) {
    if (typeof on !== 'boolean') throw invalidArgType('flag', 'boolean', on);
    native.setOption(this.#handle, 'setBroadcast', on);
  }

  setTTL(ttl) {
    if (typeof ttl !== 'number') throw invalidArgType('ttl', 'number', ttl);
    native.setOption(this.#handle, 'setTTL', ttl);
    return ttl;
  }

  setMulticastTTL(ttl) {
    if (typeof ttl !== 'number') throw invalidArgType('ttl', 'number', ttl);
    native.setOption(this.#handle, 'setMulticastTTL', ttl);
    return ttl;
  }

  setMulticastLoopback(on) {
    native.setOption(this.#handle, 'setMulticastLoopback', Boolean(on));
    return Boolean(on);
  }

  setMulticastInterface(interfaceAddress) {
    if (typeof interfaceAddress !== 'string') {
      throw invalidArgType('multicastInterface', 'string', interfaceAddress);
    }
    native.setOption(this.#handle, 'setMulticastInterface', interfaceAddress);
  }

  addMembership(multicastAddress, interfaceAddress) {
    if (typeof multicastAddress !== 'string') {
      throw invalidArgType('multicastAddress', 'string', multicastAddress);
    }
    native.membership(this.#handle, 'addMembership', multicastAddress, interfaceAddress ?? '');
  }

  dropMembership(multicastAddress, interfaceAddress) {
    if (typeof multicastAddress !== 'string') {
      throw invalidArgType('multicastAddress', 'string', multicastAddress);
    }
    native.membership(this.#handle, 'dropMembership', multicastAddress, interfaceAddress ?? '');
  }

  setRecvBufferSize(size) { native.setOption(this.#handle, 'setRecvBufferSize', size); }
  setSendBufferSize(size) { native.setOption(this.#handle, 'setSendBufferSize', size); }
  getRecvBufferSize() { return native.setOption(this.#handle, 'getRecvBufferSize', 0); }
  getSendBufferSize() { return native.setOption(this.#handle, 'getSendBufferSize', 0); }

  // Source-specific multicast needs platform support beyond the send/receive
  // path and is absent rather than silently doing nothing.

  ref() { return this; }
  unref() { return this; }

  _deliver(payload, address, port, family) {
    const message = Buffer.from(payload, 'latin1');
    if (this.#receiveBlockList?.check(address, this.#blockFamily())) return;
    this.emit('message', message, { address, family, port, size: message.length });
  }
}

// The receive loop dispatches here, once per datagram, on the isolate thread.
globalThis.__otterDgramDeliver = function deliver(handle, payload, address, port, family) {
  const socket = sockets.get(handle);
  if (socket === undefined) return;
  socket._deliver(payload, address, port, family);
};

function createSocket(options, listener) {
  return new Socket(options, listener);
}

module.exports = { createSocket, Socket };
