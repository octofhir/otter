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
  // Pausing standard input has to stop the descriptor, not only the stream:
  // an open handle nobody is reading is still a reason for the loop to wait,
  // and a program that never asks for its input must be free to finish.
  stdin.on('pause', () => {
    const handle = stdin._handle;
    if (handle === null || handle === undefined) return;
    stdin._readableState.reading = false;
    handle.reading = false;
    if (typeof handle.readStop === 'function') handle.readStop();
  });
  // It starts paused: reading begins when the program says it is listening.
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

// The program's own output, as a stream rather than a bare write.
//
// What reaches the descriptor is still the host's write, which returns only
// once the bytes are gone — a program that exits right after printing must
// find its output already there, and an asynchronous stream over the same
// descriptor could not promise that. What the stream adds is the shape
// everything else expects of `process.stdout`: it is an `EventEmitter`, it
// can be piped to and written through, and it says whether it is a terminal.
function makeStdout(fd, write) {
  const { Writable } = require('stream');
  const stream = new Writable({
    decodeStrings: false,
    write(chunk, encoding, callback) {
      try {
        write(typeof chunk === 'string' ? chunk : chunk.toString(encoding === 'buffer' ? undefined : encoding));
      } catch (error) {
        callback(error);
        return;
      }
      callback();
    },
  });
  stream.fd = fd;
  const isTTY = guessHandleType(fd) === 'TTY';
  stream.isTTY = isTTY;
  if (isTTY) {
    const { WriteStream } = require('tty');
    stream.getColorDepth = WriteStream.prototype.getColorDepth;
    stream.hasColors = WriteStream.prototype.hasColors;
    stream.columns = 80;
    stream.rows = 24;
  }
  // Standard output belongs to the process, not to this stream: ending it
  // would take the program's own output away from anything else writing to
  // the same descriptor.
  stream.end = function endStdout() { return this; };
  stream.destroy = function destroyStdout() { return this; };
  return stream;
}

module.exports = { makeStdin, makeStdout };
