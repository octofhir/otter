'use strict';

// internalBinding('tcp_wrap') — TCP handles over the host net native.

const {
  StreamHandle,
  adoptServer,
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

  // A connection that arrived over a channel is already carried by the host;
  // this is the handle that stands for it here.
  static adopt(id) {
    const handle = new TCP(constants.SOCKET);
    handle._adoptFd(id);
    return handle;
  }

  // A listening socket that arrived over a channel accepts here without a
  // second bind: the kernel already gave it its address.
  static adoptListener(id) {
    const handle = new TCP(constants.SERVER);
    adoptServer(handle, id);
    return handle;
  }

  bind(address, port) {
    this._boundAddress = address;
    this._boundPort = port >>> 0;
    return 0;
  }

  bind6(address, port, flags) {
    this._ipv6Only = ((flags | 0) & constants.UV_TCP_IPV6ONLY) !== 0;
    return this.bind(address, port);
  }

  listen(_backlog) {
    if (this._serverId !== -1) return 0;
    const address = this._boundAddress ?? '0.0.0.0';
    const port = this._boundPort;
    const err = startListen(this, () => native.listen(address, port, this._ipv6Only === true));
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

  // A bound handle dials from the address it was bound to: `connect`
  // carries the near end so the kernel binds it before connecting.
  connect(req, address, port) {
    return startConnect(this, req, (token) => native.connect(
      address,
      port,
      token,
      this._boundAddress ?? '',
      this._boundPort,
    ));
  }

  connect6(req, address, port) {
    return this.connect(req, address, port);
  }
}

class TCPConnectWrap {
  getAsyncId() { return -1; }
}

module.exports = { TCP, TCPConnectWrap, constants };
