'use strict';

class ReadStream {
  constructor(fd) {
    this.fd = fd;
    this.isTTY = false;
  }
  setRawMode() { return this; }
}

class WriteStream {
  constructor(fd) {
    this.fd = fd;
    this.isTTY = false;
  }
  getColorDepth(env = process.env) {
    return colorDepth(this?.isTTY === true, env);
  }
  hasColors(count, env) {
    if (count !== null && typeof count === 'object') {
      env = count;
      count = 16;
    }
    if (count === undefined) count = 16;
    // `this` may be any writable the caller grafted the method onto
    // (Node's own tests copy it straight onto process.stderr).
    return Number(count) <= 2 ** colorDepth(this?.isTTY === true, env);
  }
}

// Node's color negotiation: NO_COLOR and FORCE_COLOR override the terminal,
// then the TTY answers for itself.
function colorDepth(isTTY, env = process.env) {
  if (env.NO_COLOR !== undefined && env.NO_COLOR !== '') return 1;
  const force = env.FORCE_COLOR;
  if (force !== undefined && force !== '') {
    if (force === '0' || force === 'false') return 1;
    if (force === '2') return 8;
    if (force === '3') return 24;
    return 4;
  }
  return isTTY ? 4 : 1;
}

function isatty() { return false; }

module.exports = { ReadStream, WriteStream, isatty };
