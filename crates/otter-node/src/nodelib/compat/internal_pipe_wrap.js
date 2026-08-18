'use strict';

// internalBinding('pipe_wrap') — Unix-domain socket handles over the host
// net native's path-based dial/listen surface.

const {
  StreamHandle,
  startConnect,
  startListen,
  handleClassByServer,
} = require('internal/otter/stream_handle');
const native = require('internal/otter/net');

const constants = {
  SOCKET: 0,
  SERVER: 1,
  IPC: 2,
  UV_READABLE: 1,
  UV_WRITABLE: 2,
};

class Pipe extends StreamHandle {
  constructor(type) {
    super();
    this.type = type;
    this.ipc = type === constants.IPC;
    this._boundPath = null;
  }

  bind(path) {
    this._boundPath = path;
    return 0;
  }

  listen(_backlog) {
    const path = this._boundPath;
    const err = startListen(this, () => native.listenPath(path));
    if (err === 0) {
      handleClassByServer.set(this._serverId, Pipe);
      this._boundAddress = path;
    }
    return err;
  }

  connect(req, path) {
    return startConnect(this, req, (token) => native.connectPath(path, token));
  }

  // Pipe names are the path; the native id table answers 'local' with the
  // bound path for path listeners.
  getsockname(out) {
    if (this._boundPath !== null) {
      out.address = this._boundPath;
      return 0;
    }
    return super.getsockname(out);
  }
}

class PipeConnectWrap {
  getAsyncId() { return -1; }
}

module.exports = { Pipe, PipeConnectWrap, constants };
