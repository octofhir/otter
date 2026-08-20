'use strict';

// The program's own standard streams.
//
// A descriptor says what it is, and what it is decides the stream it becomes:
// a terminal and a pipe are read on the loop through the host's stream table,
// a redirected file is read through the file system, because a file is not
// something a poller can wait on. This is the same decision Node's own
// bootstrap makes, and it is made once, the first time the program asks.

const { guessHandleType } = require('internal/util');

function readableStream(fd) {
  switch (guessHandleType(fd)) {
    case 'TTY': {
      const tty = require('tty');
      return new tty.ReadStream(fd);
    }
    case 'FILE': {
      const fs = require('fs');
      // The descriptor belongs to the process, not to the stream: closing it
      // would take the program's own input away from anything else reading it.
      return new fs.ReadStream(null, { fd, autoClose: false });
    }
    case 'PIPE':
    case 'TCP': {
      const net = require('net');
      return new net.Socket({ fd, readable: true, writable: false });
    }
    default: {
      const { ERR_UNKNOWN_STREAM_TYPE } = require('internal/errors').codes;
      throw new ERR_UNKNOWN_STREAM_TYPE(fd);
    }
  }
}

function makeStdin() {
  const stdin = readableStream(0);
  stdin.fd = 0;
  // Standard input starts paused: a program that never reads it must not be
  // held open by it, and one that reads it says so by listening or resuming.
  stdin.pause();
  // `process.stdin` is the program's, not a connection the program owns:
  // destroying it must not take the descriptor away from whatever else in the
  // process is reading the same input.
  const destroy = stdin.destroy;
  stdin.destroy = function destroyStdin(...args) {
    if (this._handle === null || this._handle === undefined) return this;
    return destroy.apply(this, args);
  };
  return stdin;
}

module.exports = { makeStdin };
