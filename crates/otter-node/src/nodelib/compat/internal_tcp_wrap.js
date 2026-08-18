'use strict';

// internalBinding('tcp_wrap') — TCP handles over the host net native.

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
  UV_TCP_IPV6ONLY: 1,
};

class TCP extends StreamHandle {
  constructor(type) {
    super();
    this.type = type;
  }

  bind(address, port) {
    this._boundAddress = address;
    this._boundPort = port >>> 0;
    return 0;
  }

  bind6(address, port, _flags) {
    return this.bind(address, port);
  }

  listen(_backlog) {
    const address = this._boundAddress ?? '0.0.0.0';
    const port = this._boundPort;
    const err = startListen(this, () => native.listen(address, port));
    if (err === 0) {
      handleClassByServer.set(this._serverId, TCP);
      const name = native.address(this._serverId, 'local');
      if (name !== undefined && name !== null) {
        this._boundAddress = name.address;
        this._boundPort = name.port;
      }
    }
    return err;
  }

  connect(req, address, port) {
    return startConnect(this, req, (token) => native.connect(address, port, token));
  }

  connect6(req, address, port) {
    return this.connect(req, address, port);
  }
}

class TCPConnectWrap {
  getAsyncId() { return -1; }
}

module.exports = { TCP, TCPConnectWrap, constants };
