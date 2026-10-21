'use strict';

// The program's own standard streams.
//
// A descriptor says what it is, and what it is decides the stream it becomes:
// a terminal and a pipe are read on the loop through the host's stream table,
// a redirected file is read through the file system, because a file is not
// something a poller can wait on. This is the same decision Node's own
// bootstrap makes, and it is made once, the first time the program asks.

// Asked only when a stream is built: the console reaches the descriptors
// through this module long before, and needs none of `internal/util`.
function guessHandleType(fd) {
  return require('internal/util').guessHandleType(fd);
}

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

// The one stream each standard output becomes, built the first time anything
// asks for it — `process.stdout`'s getter, or the console that has been
// writing to the descriptor directly — and the same stream ever after.
const defaultOutputs = [];

// The program's own output, as a stream rather than a bare write.
//
// What reaches the descriptor is still the host's write, which returns only
// once the bytes are gone — a program that exits right after printing must
// find its output already there, and an asynchronous stream over the same
// descriptor could not promise that. What the stream adds is the shape
// everything else expects of `process.stdout`: it is an `EventEmitter`, it
// can be piped to and written through, and it says whether it is a terminal.
function makeStdout(fd, write) {
  return defaultOutputs[fd] ??= buildStdout(fd, write);
}

// The default stream of `fd` if something has built it.
function builtOutput(fd) {
  return defaultOutputs[fd];
}

// The default stream of `fd`, built over the runtime's own write when nothing
// has asked for it yet.
function defaultOutput(fd) {
  return defaultOutputs[fd] ??=
    buildStdout(fd, (chunk) => module.exports.write(fd, chunk));
}

function buildStdout(fd, write) {
  const { Writable } = require('stream');
  const stream = new Writable({
    decodeStrings: false,
    write(chunk, encoding, callback) {
      try {
        // Bytes reach the descriptor as bytes. A string carries the encoding
        // it was written in, so it becomes bytes here; everything else already
        // is a buffer source and travels untouched, which is what a program
        // writing a binary protocol down this stream depends on.
        write(
          typeof chunk === 'string' && encoding && encoding !== 'utf8' && encoding !== 'buffer'
            ? Buffer.from(chunk, encoding)
            : chunk,
        );
      } catch (error) {
        callback(error);
        return;
      }
      callback();
    },
  });
  const traits = describeOutput(fd, guessHandleType(fd) === 'TTY');
  for (const key in traits) stream[key] = traits[key];
  // Standard output belongs to the process, not to this stream: ending it
  // would take the program's own output away from anything else writing to
  // the same descriptor.
  stream.end = function endStdout() { return this; };
  stream.destroy = function destroyStdout() { return this; };
  return stream;
}

// What an output stream says about its descriptor: which one it is, whether
// it is a terminal, and for a terminal its color support, its size and the
// cursor and size members a `tty.WriteStream` has, so the program's own
// output answers every call a terminal stream it made itself would. The
// stream carries exactly these, and the console decides colors from them
// alone while no stream has been built yet.
//
// None of this loads `tty`: that module stands on the whole `net` stack, and
// the first line a program prints to a terminal must not pay for it.
function describeOutput(fd, isTTY) {
  const traits = { __proto__: null, fd, isTTY };
  if (isTTY) {
    const { getColorDepth, hasColors } = require('internal/tty');
    traits.getColorDepth = getColorDepth;
    traits.hasColors = hasColors;
    // A terminal that will not say its size is left without one, as Node
    // leaves it.
    const size = [0, 0];
    if (require('internal/otter/tty').getWindowSize(fd, size) === 0) {
      traits.columns = size[0];
      traits.rows = size[1];
    }
    for (const key in terminalMembers) traits[key] = terminalMembers[key];
  }
  return traits;
}

let readline;
function lazyReadline() {
  readline ??= require('readline');
  return readline;
}

// `tty.WriteStream`'s own members, over the descriptor rather than a handle.
const terminalMembers = {
  __proto__: null,
  getWindowSize() {
    return [this.columns, this.rows];
  },
  // Node's stream reads its size again on SIGWINCH; a program may ask for
  // the same at any time.
  _refreshSize() {
    const size = [0, 0];
    const err = require('internal/otter/tty').getWindowSize(this.fd, size);
    if (err !== 0) {
      const { ErrnoException } = require('internal/errors');
      this.emit('error', new ErrnoException(err, 'getWindowSize'));
      return;
    }
    if (this.columns !== size[0] || this.rows !== size[1]) {
      this.columns = size[0];
      this.rows = size[1];
      this.emit('resize');
    }
  },
  cursorTo(x, y, callback) {
    return lazyReadline().cursorTo(this, x, y, callback);
  },
  moveCursor(dx, dy, callback) {
    return lazyReadline().moveCursor(this, dx, dy, callback);
  },
  clearLine(dir, callback) {
    return lazyReadline().clearLine(this, dir, callback);
  },
  clearScreenDown(callback) {
    return lazyReadline().clearScreenDown(this, callback);
  },
};

// The runtime defines `holdsAccessor`, `streamUnset`, `isatty` and `write`
// beside these: the console's way to the descriptors before any stream exists.
module.exports = {
  makeStdin,
  makeStdout,
  builtOutput,
  defaultOutput,
  describeOutput,
};
