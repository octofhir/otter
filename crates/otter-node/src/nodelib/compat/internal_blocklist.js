'use strict';

// `internal/blocklist` — address deny rules for `net.BlockList`
// (§https://nodejs.org/api/net.html#class-netblocklist). Pure-JS rule
// table: v4 addresses map to a 32-bit integer, v6 to a 128-bit BigInt,
// and a v4 rule also matches its ::ffff: v6 mapping the way Node's does.

const { ERR_INVALID_ARG_TYPE } = require('internal/errors').codes;
const { parseV4, parseV6 } = require('internal/otter/ip');

function blockKey(addr, family) {
  if (String(family).toLowerCase() === 'ipv6') {
    const v6 = parseV6(addr);
    if (v6 === null) return null;
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
    if (typeof address === 'object' && address !== null && 'address' in address) {
      family = address.family;
      address = address.address;
    }
    const key = blockKey(address, family);
    if (key === null) throw new ERR_INVALID_ARG_TYPE('address', 'a valid IP address', address);
    this.#rules.push({ kind: 'Address', family: key.family, start: key.value, end: key.value, text: `Address: ${key.family.toUpperCase()} ${address}` });
  }

  addRange(start, end, family = 'ipv4') {
    if (typeof start === 'object' && start !== null && 'address' in start) {
      family = start.family;
      start = start.address;
    }
    if (typeof end === 'object' && end !== null && 'address' in end) end = end.address;
    const from = blockKey(start, family);
    const to = blockKey(end, family);
    if (from === null || to === null || from.family !== to.family) {
      throw new ERR_INVALID_ARG_TYPE('start', 'a valid IP range', start);
    }
    this.#rules.push({ kind: 'Range', family: from.family, start: from.value, end: to.value, text: `Range: ${from.family.toUpperCase()} ${start}-${end}` });
  }

  addSubnet(network, prefix, family = 'ipv4') {
    if (typeof network === 'object' && network !== null && 'address' in network) {
      family = network.family;
      network = network.address;
    }
    const key = blockKey(network, family);
    if (key === null) throw new ERR_INVALID_ARG_TYPE('network', 'a valid IP address', network);
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
    if (typeof address === 'object' && address !== null && 'address' in address) {
      family = address.family;
      address = address.address;
    }
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

module.exports = { BlockList };
