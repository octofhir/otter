'use strict';

// StreamBase-analog handle over the host net native. `tcp_wrap` and
// `pipe_wrap` subclass `StreamHandle`; this module owns the handle tables
// and installs the host event dispatcher the native loops call into
// (`__otterNetDeliver`).
//
// The native surface delivers reads eagerly; `readStop` parks incoming
// chunks in a per-handle queue that `readStart` replays in order, so the
// `onread` contract (including `streamBaseState` words and EOF) matches
// what vendored `internal/stream_base_commons` expects. Writes complete
// synchronously — `kLastWriteWasAsync` stays 0 and `oncomplete` is never
// invoked for them, which is the documented sync-write path.

const { internalBinding } = require('internal/bootstrap/realm');
const {
  streamBaseState,
  kReadBytesOrError,
  kArrayBufferOffset,
  kBytesWritten,
  kLastWriteWasAsync,
} = internalBinding('stream_wrap');
const uv = internalBinding('uv');
const { Buffer } = require('buffer');

const native = require('internal/otter/net');

// One live wrap per native connection id / server id / connect token.
const connections = new Map();
const servers = new Map();
const pendingConnects = new Map();
let nextConnectToken = 1;

function uvCode(codeName) {
  const numeric = uv[`UV_${codeName}`];
  if (typeof numeric === 'number') return numeric;
  return uv.UV_UNKNOWN ?? -4094;
}

class StreamHandle {
  constructor() {
    this.fd = -1;
    this.reading = false;
    this.onread = null;
    this.onconnection = null;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this._parkedChunks = [];
    this._eofPending = false;
    this._eofDelivered = false;
    this._closed = false;
    this._serverId = -1;
    this._boundAddress = null;
    this._boundPort = 0;
    this._refed = true;
    this._userBuffer = null;
  }

  getAsyncId() { return -1; }

  // ---- lifecycle ----

  _adoptFd(fd) {
    this.fd = fd;
    connections.set(fd, this);
    if (!this._refed) native.hold(fd, false);
  }

  close(callback) {
    if (this._closed) {
      if (typeof callback === 'function') queueMicrotask(callback);
      return;
    }
    this._closed = true;
    if (this.fd !== -1) {
      connections.delete(this.fd);
      try { native.close(this.fd); } catch { /* already gone */ }
      this.fd = -1;
    }
    if (this._serverId !== -1) {
      servers.delete(this._serverId);
      try { native.close(this._serverId); } catch { /* already gone */ }
      this._serverId = -1;
    }
    if (typeof callback === 'function') queueMicrotask(callback);
  }

  ref() {
    this._refed = true;
    const id = this.fd !== -1 ? this.fd : this._serverId;
    if (id !== -1) native.hold(id, true);
  }

  unref() {
    this._refed = false;
    const id = this.fd !== -1 ? this.fd : this._serverId;
    if (id !== -1) native.hold(id, false);
  }

  // ---- reading ----

  readStart() {
    this.reading = true;
    if (this._parkedChunks.length > 0 || this._eofPending) {
      queueMicrotask(() => this._drainParked());
    }
    return 0;
  }

  readStop() {
    this.reading = false;
    return 0;
  }

  useUserBuffer(buffer) {
    this._userBuffer = buffer;
    return true;
  }

  _drainParked() {
    while (this.reading && this._parkedChunks.length > 0) {
      this._deliverChunk(this._parkedChunks.shift());
    }
    if (this.reading && this._parkedChunks.length === 0 && this._eofPending) {
      this._eofPending = false;
      this._deliverEof();
    }
  }

  _onData(latin1Payload) {
    const chunk = Buffer.from(latin1Payload, 'latin1');
    if (!this.reading || this._parkedChunks.length > 0) {
      this._parkedChunks.push(chunk);
      return;
    }
    this._deliverChunk(chunk);
  }

  _onEnd() {
    if (!this.reading || this._parkedChunks.length > 0) {
      this._eofPending = true;
      return;
    }
    this._deliverEof();
  }

  _deliverChunk(chunk) {
    if (typeof this.onread !== 'function' || this._closed) return;
    // Counts only bytes handed to `onread`: a paused handle parks chunks
    // and reports 0, the way an unstarted libuv reader would.
    this.bytesRead += chunk.length;
    streamBaseState[kReadBytesOrError] = chunk.length;
    streamBaseState[kArrayBufferOffset] = chunk.byteOffset;
    this.onread(chunk.buffer);
  }

  _deliverEof() {
    if (this._eofDelivered || this._closed) return;
    this._eofDelivered = true;
    if (typeof this.onread !== 'function') return;
    streamBaseState[kReadBytesOrError] = uv.UV_EOF;
    streamBaseState[kArrayBufferOffset] = 0;
    this.onread(undefined);
  }

  // ---- writing ----

  _writeBytes(buffer) {
    if (this.fd === -1 || this._closed) return uvCode('EBADF');
    try {
      native.write(this.fd, buffer);
    } catch {
      return uvCode('EPIPE');
    }
    this.bytesWritten += buffer.length;
    streamBaseState[kBytesWritten] = buffer.length;
    streamBaseState[kLastWriteWasAsync] = 0;
    return 0;
  }

  writeBuffer(req, buffer) { return this._writeBytes(buffer); }
  writeUtf8String(req, data) { return this._writeBytes(Buffer.from(data, 'utf8')); }
  writeLatin1String(req, data) { return this._writeBytes(Buffer.from(data, 'latin1')); }
  writeAsciiString(req, data) { return this._writeBytes(Buffer.from(data, 'ascii')); }
  writeUcs2String(req, data) { return this._writeBytes(Buffer.from(data, 'ucs2')); }

  writev(req, chunks, allBuffers) {
    const parts = [];
    if (allBuffers) {
      for (const chunk of chunks) parts.push(chunk);
    } else {
      for (let i = 0; i < chunks.length; i += 2) {
        const data = chunks[i];
        const encoding = chunks[i + 1];
        parts.push(typeof data === 'string' ? Buffer.from(data, encoding) : data);
      }
    }
    return this._writeBytes(Buffer.concat(parts));
  }

  shutdown(req) {
    if (this.fd === -1 || this._closed) return uvCode('ENOTCONN');
    const fd = this.fd;
    queueMicrotask(() => {
      let status = 0;
      try {
        native.end(fd);
      } catch {
        status = uvCode('ENOTCONN');
      }
      if (typeof req.oncomplete === 'function') req.oncomplete.call(req, status);
    });
    return 0;
  }

  // ---- names ----

  _fillName(out, which) {
    const id = this.fd !== -1 ? this.fd : this._serverId;
    if (id === -1) {
      if (this._boundAddress !== null) {
        out.address = this._boundAddress;
        out.port = this._boundPort;
        out.family = this._boundAddress.includes(':') ? 'IPv6' : 'IPv4';
        return 0;
      }
      return uvCode('EBADF');
    }
    const name = native.address(id, which);
    if (name === undefined || name === null) return uvCode('ENOTCONN');
    Object.assign(out, name);
    return 0;
  }

  getsockname(out) { return this._fillName(out, 'local'); }
  getpeername(out) { return this._fillName(out, 'remote'); }

  // ---- socket options (accepted, mostly no-ops on the host) ----

  setNoDelay(enable) {
    if (this.fd !== -1) native.setOption(this.fd, 'setNoDelay', enable !== false);
    return 0;
  }

  setKeepAlive(_enable, _delay) { return 0; }
  setSimultaneousAccepts(_enable) { return 0; }
  setBlocking(_blocking) { return 0; }
  setTypeOfService(_tos) { return 0; }
  getTypeOfService() { return 0; }
  setPendingInstances(_instances) { return 0; }
  fchmod(_mode) { return 0; }

  open(_fd) { return uvCode('ENOTSUP'); }
  reset(callback) { this.close(callback); return 0; }
}

// ---- connect / listen plumbing shared by TCP and Pipe ----

function startConnect(handle, req, dial) {
  const token = nextConnectToken++;
  pendingConnects.set(token, { handle, req });
  try {
    dial(token);
  } catch (error) {
    pendingConnects.delete(token);
    const status = uvCode(error?.code ?? 'ECONNREFUSED');
    queueMicrotask(() => {
      if (typeof req.oncomplete === 'function') {
        req.oncomplete(status, handle, req, false, false);
      }
    });
  }
  return 0;
}

function startListen(handle, bind) {
  let bound;
  try {
    bound = bind();
  } catch (error) {
    return uvCode(error?.code ?? 'EADDRINUSE');
  }
  handle._serverId = bound.handle;
  servers.set(bound.handle, handle);
  if (bound.address !== undefined) {
    handle._boundAddress = bound.address;
    handle._boundPort = bound.port ?? 0;
  }
  if (!handle._refed) native.hold(bound.handle, false);
  return 0;
}

function makeConnectionHandle(HandleClass, fd, remote) {
  const client = new HandleClass();
  client._adoptFd(fd);
  if (remote !== undefined && remote !== null && remote.address !== undefined) {
    client._remoteName = remote;
  }
  return client;
}

// The native loops dispatch here, on the isolate thread. Handle classes
// register their constructors so accepts materialize the right wrap type.
const handleClassByServer = new Map();

globalThis.__otterNetDeliver = function deliver(kind, first, second, third) {
  if (kind === 'accept') {
    const server = servers.get(first);
    if (server === undefined) {
      try { native.close(second); } catch { /* orphan */ }
      return;
    }
    const HandleClass = handleClassByServer.get(first) ?? server.constructor;
    const client = makeConnectionHandle(HandleClass, second, third);
    if (typeof server.onconnection === 'function') {
      server.onconnection(0, client);
    } else {
      client.close();
    }
    return;
  }
  if (kind === 'connect') {
    const entry = pendingConnects.get(first);
    pendingConnects.delete(first);
    if (entry === undefined) {
      try { native.close(second); } catch { /* orphan */ }
      return;
    }
    const { handle, req } = entry;
    if (handle._closed) {
      try { native.close(second); } catch { /* orphan */ }
      return;
    }
    handle._adoptFd(second);
    if (typeof req.oncomplete === 'function') {
      req.oncomplete(0, handle, req, true, true);
    }
    return;
  }
  if (kind === 'connectError') {
    const entry = pendingConnects.get(first);
    pendingConnects.delete(first);
    if (entry === undefined) return;
    const { handle, req } = entry;
    const status = uvCode(second);
    if (typeof req.oncomplete === 'function') {
      req.oncomplete(status, handle, req, false, false);
    }
    return;
  }
  const handle = connections.get(first);
  if (handle === undefined) return;
  if (kind === 'data') handle._onData(second);
  else if (kind === 'end') handle._onEnd();
};
// Off the enumerable global surface: the Node harness flags unknown
// enumerable globals as leaks.
Object.defineProperty(globalThis, '__otterNetDeliver', { enumerable: false });

module.exports = {
  StreamHandle,
  startConnect,
  startListen,
  handleClassByServer,
  uvCode,
};
