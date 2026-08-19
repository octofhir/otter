'use strict';

// internalBinding('udp_wrap') — UDP handles over the host dgram native.
//
// The native half owns the socket and the receive loop, keyed by a numeric
// id; this file is the handle contract the vendored `dgram` drives: bind,
// send through a request object whose `oncomplete` reports the status,
// `recvStart`/`recvStop` around an `onmessage` callback, and the option and
// membership calls, each answering a libuv-shaped return code rather than
// throwing.

const { Buffer } = require('buffer');
const { internalBinding } = require('internal/bootstrap/realm');
const uv = internalBinding('uv');
const native = require('internal/otter/dgram');

const handles = new Map();

function uvCode(codeName) {
  const numeric = uv[`UV_${codeName}`];
  return typeof numeric === 'number' ? numeric : (uv.UV_UNKNOWN ?? -4094);
}

function codeOf(error) {
  return uvCode(error?.code ?? 'EINVAL');
}

function toLatin1(chunk) {
  if (typeof chunk === 'string') return Buffer.from(chunk, 'utf8').toString('latin1');
  if (Buffer.isBuffer(chunk)) return chunk.toString('latin1');
  if (ArrayBuffer.isView(chunk)) {
    return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength).toString('latin1');
  }
  if (chunk instanceof ArrayBuffer) return Buffer.from(chunk).toString('latin1');
  return String(chunk);
}

class SendWrap {
  constructor() {
    this.oncomplete = null;
    this.address = '';
    this.port = 0;
    this.callback = null;
    this.req = null;
  }
  getAsyncId() { return -1; }
}

class UDP {
  constructor() {
    this.fd = -1;
    this.type = 'udp4';
    this.onmessage = null;
    this.reading = false;
    this.lookup = null;
    this._closed = false;
    this._parked = [];
    this._refed = true;
    this._remote = null;
    this._local = null;
  }

  getAsyncId() { return -1; }

  // ---- lifetime ----

  bind(address, port, flags) {
    if (this.fd !== -1) return uvCode('EINVAL');
    let bound;
    try {
      bound = native.bind(this.type, port >>> 0, address ?? '', flags | 0);
    } catch (error) {
      return codeOf(error);
    }
    this.fd = bound.handle;
    this._local = { address: bound.address, port: bound.port, family: bound.family };
    handles.set(this.fd, this);
    if (!this._refed) native.hold?.(this.fd, false);
    return 0;
  }

  // `internal/dgram` rebinds `bind`/`connect`/`send` to these on a v6
  // handle, so the v6 form must call the prototype method rather than the
  // instance one it just replaced.
  bind6(address, port, flags) {
    this.type = 'udp6';
    return UDP.prototype.bind.call(this, address, port, flags);
  }

  connect(address, port) {
    if (this.fd === -1) {
      const err = this.bind('', 0, 0);
      if (err !== 0) return err;
    }
    let resolved;
    try {
      resolved = native.resolve(port >>> 0, address ?? '', this.type);
    } catch (error) {
      return codeOf(error);
    }
    if (resolved === undefined || resolved === null) return uvCode('ENOTFOUND');
    this._remote = resolved;
    return 0;
  }

  connect6(address, port) {
    this.type = 'udp6';
    return UDP.prototype.connect.call(this, address, port);
  }

  disconnect() {
    if (this._remote === null) return uvCode('ENOTCONN');
    this._remote = null;
    return 0;
  }

  close(callback) {
    if (this.fd !== -1) {
      handles.delete(this.fd);
      try { native.close(this.fd); } catch { /* already gone */ }
    }
    this.fd = -1;
    this._closed = true;
    this.reading = false;
    if (typeof callback === 'function') setImmediate(callback);
    return 0;
  }

  ref() {
    this._refed = true;
    if (this.fd !== -1) native.hold?.(this.fd, true);
  }

  unref() {
    this._refed = false;
    if (this.fd !== -1) native.hold?.(this.fd, false);
  }

  // ---- reading ----

  recvStart() {
    if (this.fd === -1) return uvCode('EBADF');
    this.reading = true;
    if (this._parked.length > 0) {
      const parked = this._parked;
      this._parked = [];
      for (const datagram of parked) this._deliver(datagram);
    }
    return 0;
  }

  recvStop() {
    this.reading = false;
    return 0;
  }

  _deliver(datagram) {
    if (!this.reading) {
      this._parked.push(datagram);
      return;
    }
    if (typeof this.onmessage !== 'function') return;
    const buffer = Buffer.from(datagram.payload, 'latin1');
    this.onmessage(buffer.length, this, buffer, {
      address: datagram.address,
      family: datagram.family,
      port: datagram.port,
      size: buffer.length,
    });
  }

  // ---- writing ----

  send(req, list, count, port, address, _hasCallback) {
    if (this.fd === -1) {
      const err = this.bind('', 0, 0);
      if (err !== 0) return err;
    }
    let payload = '';
    for (let i = 0; i < count; i++) {
      const chunk = list[i];
      if (chunk === undefined || chunk === null) continue;
      payload += toLatin1(chunk);
    }
    const target = this._remote ?? { address: address ?? '', port: port >>> 0 };
    let status = 0;
    try {
      native.send(this.fd, payload, target.port >>> 0, target.address ?? '');
    } catch (error) {
      status = codeOf(error);
    }
    // libuv answers the send asynchronously even when the datagram left
    // synchronously, so the request completes on the next turn. The
    // callback reads `this` as the request and takes (status, length).
    const length = payload.length;
    setImmediate(() => {
      if (typeof req.oncomplete === 'function') {
        Reflect.apply(req.oncomplete, req, [status, length]);
      }
    });
    return 0;
  }

  send6(req, list, count, port, address, hasCallback) {
    this.type = 'udp6';
    return UDP.prototype.send.call(this, req, list, count, port, address, hasCallback);
  }

  // ---- names ----

  getsockname(out) {
    if (this.fd === -1) return uvCode('EBADF');
    let name = this._local;
    try {
      name = native.address(this.fd) ?? name;
    } catch {
      // The bind result already named the socket; a later query failing
      // does not un-name it.
    }
    if (name === undefined || name === null) return uvCode('EBADF');
    Object.assign(out, name);
    return 0;
  }

  getpeername(out) {
    if (this._remote === null) return uvCode('ENOTCONN');
    Object.assign(out, this._remote);
    return 0;
  }

  // ---- options ----

  _option(name, value, extra) {
    if (this.fd === -1) return uvCode('EBADF');
    try {
      native.setOption(this.fd, name, value, extra);
    } catch (error) {
      return codeOf(error);
    }
    return 0;
  }

  setBroadcast(on) { return this._option('setBroadcast', on !== 0); }
  setTTL(ttl) { return this._option('setTTL', ttl); }
  setMulticastTTL(ttl) { return this._option('setMulticastTTL', ttl); }
  setMulticastLoopback(on) { return this._option('setMulticastLoopback', on !== 0); }
  setMulticastInterface(iface) { return this._option('setMulticastInterface', iface); }

  bufferSize(size, isSend, _isBuffer) {
    const name = isSend ? 'setSendBufferSize' : 'setRecvBufferSize';
    if (size === 0) {
      // A zero size is a read of the current one.
      try {
        return native.setOption(this.fd, isSend ? 'getSendBufferSize' : 'getRecvBufferSize', 0);
      } catch (error) {
        return codeOf(error);
      }
    }
    const err = this._option(name, size);
    return err === 0 ? size : err;
  }

  _membership(operation, multicast, iface, source) {
    // libuv answers a membership call on an unbound socket with EINVAL:
    // there is a handle, it simply has no address yet.
    if (this.fd === -1) return uvCode('EINVAL');
    try {
      native.membership(this.fd, operation, multicast, iface ?? '', source ?? '');
    } catch (error) {
      return codeOf(error);
    }
    return 0;
  }

  addMembership(multicast, iface) { return this._membership('add', multicast, iface); }
  dropMembership(multicast, iface) { return this._membership('drop', multicast, iface); }
  addSourceSpecificMembership(source, group, iface) {
    return this._membership('addSource', group, iface, source);
  }
  dropSourceSpecificMembership(source, group, iface) {
    return this._membership('dropSource', group, iface, source);
  }
}

// The receive loop dispatches here, once per datagram, on the isolate thread.
globalThis.__otterDgramDeliver = function deliver(handle, payload, address, port, family) {
  const udp = handles.get(handle);
  if (udp === undefined) return;
  udp._deliver({ payload, address, port, family });
};
Object.defineProperty(globalThis, '__otterDgramDeliver', { enumerable: false });

module.exports = {
  UDP,
  SendWrap,
  constants: {
    UV_UDP_IPV6ONLY: 1,
    UV_UDP_REUSEADDR: 4,
    UV_UDP_REUSEPORT: 8,
  },
};
