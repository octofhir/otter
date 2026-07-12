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
  getColorDepth() { return this.isTTY ? 4 : 1; }
  hasColors(count = 16) { return this.isTTY && Number(count) <= 16; }
}

function isatty() { return false; }

module.exports = { ReadStream, WriteStream, isatty };
