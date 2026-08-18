'use strict';

// `internal/socketaddress` — the immutable address/family/port/flowlabel
// value type `net.SocketAddress` exposes
// (§https://nodejs.org/api/net.html#class-netsocketaddress).

const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE } = require('internal/errors').codes;
const { validateObject, validateString, validatePort, validateUint32 } = require('internal/validators');
const { parseV4, parseV6 } = require('internal/otter/ip');

const kDetail = Symbol('kDetail');

class SocketAddress {
  constructor(options = {}) {
    validateObject(options, 'options');
    const { family = 'ipv4', address = (String(family).toLowerCase() === 'ipv6' ? '::' : '127.0.0.1'), port = 0, flowlabel = 0 } = options;
    validateString(address, 'options.address');
    validatePort(port, 'options.port');
    validateUint32(flowlabel, 'options.flowlabel');
    const normalizedFamily = String(family).toLowerCase();
    if (normalizedFamily !== 'ipv4' && normalizedFamily !== 'ipv6') {
      throw new ERR_INVALID_ARG_VALUE('options.family', family);
    }
    const parsed = normalizedFamily === 'ipv6' ? parseV6(address) : parseV4(address);
    if (parsed === null) {
      throw new ERR_INVALID_ARG_VALUE('options.address', address);
    }
    this[kDetail] = {
      address,
      family: normalizedFamily,
      port: port | 0,
      flowlabel: flowlabel >>> 0,
    };
  }

  get address() { return this[kDetail].address; }
  get family() { return this[kDetail].family; }
  get port() { return this[kDetail].port; }
  get flowlabel() { return this[kDetail].flowlabel; }

  toJSON() {
    return { ...this[kDetail] };
  }

  static parse(input) {
    validateString(input, 'input');
    // `host:port` for v4, `[v6]:port` or a bare address for v6.
    const bracket = /^\[([^\]]+)\](?::(\d+))?$/.exec(input);
    if (bracket !== null) {
      if (parseV6(bracket[1]) === null) return undefined;
      return new SocketAddress({ family: 'ipv6', address: bracket[1], port: Number(bracket[2] ?? 0) });
    }
    const lastColon = input.lastIndexOf(':');
    if (lastColon !== -1 && input.indexOf(':') === lastColon) {
      const host = input.slice(0, lastColon);
      const port = Number(input.slice(lastColon + 1));
      if (parseV4(host) !== null && Number.isInteger(port) && port >= 0 && port <= 65535) {
        return new SocketAddress({ family: 'ipv4', address: host, port });
      }
      return undefined;
    }
    if (parseV4(input) !== null) return new SocketAddress({ family: 'ipv4', address: input });
    if (parseV6(input) !== null) return new SocketAddress({ family: 'ipv6', address: input });
    return undefined;
  }

  static isSocketAddress(value) {
    return value instanceof SocketAddress;
  }
}

module.exports = { SocketAddress, kDetail };
