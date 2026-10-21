'use strict';

// internalBinding('tty_wrap') — terminal handles over the host stream table.
//
// A `TTY` takes its descriptor over when it is made, the way libuv's
// `uv_tty_init` does: a terminal is reopened as a description of this
// process's own, a pipe or a socket is carried as it is, and what a poller
// cannot wait on — a regular file, a device that is not a terminal, a
// descriptor that is not open — is refused with EINVAL through `ctx`, which
// is how `tty.ReadStream` and `tty.WriteStream` learn of it. A descriptor the
// run may not use — any but its standard streams — is refused the same way,
// as one the process does not have, whether the handle is being made or is
// asked to `open` one later; the natives refuse it on their own as well,
// whatever `fd` a handle is later given. Beyond the stream it carries
// what only a terminal has: raw mode, the window size, and writes that
// complete before they return.
//
// A terminal switched to raw mode is the run's to hand back, and the runtime
// does that itself: the native that changes a terminal's mode keeps the mode
// it replaced in the run's own record, out of reach of JavaScript, and the
// run hands it back once the 'exit' listeners have run, and before
// `abort()`, so a program that exits while raw does not leave its shell raw.

const { internalBinding } = require('internal/bootstrap/realm');
const {
  streamBaseState,
  kBytesWritten,
  kLastWriteWasAsync,
} = internalBinding('stream_wrap');
const uv = internalBinding('uv');
const { StreamHandle } = require('internal/otter/stream_handle');
const { guessHandleType } = require('internal/util');
const native = require('internal/otter/tty');

function isTTY(fd) {
  return native.isatty(fd);
}

// What `CollectUVExceptionInfo` puts on the context object a failed
// construction reports through.
function reportInitFailure(ctx, err) {
  if (ctx === null || typeof ctx !== 'object') return;
  ctx.errno = err;
  ctx.code = uv.errname(err);
  ctx.message = uv.getErrorMessage(err);
  ctx.syscall = 'uv_tty_init';
}

class TTY extends StreamHandle {
  constructor(fd, ctx) {
    super();
    this._raw = false;
    // The mode a switch to raw replaced, as `getMode` answered it.
    this._normalMode = null;
    this._blocking = false;
    this._terminal = false;
    const kind = native.mayUse(fd) ? guessHandleType(fd) : 'UNKNOWN';
    if (kind === 'FILE' || kind === 'UNKNOWN') {
      reportInitFailure(ctx, uv.UV_EINVAL);
      return;
    }
    this._terminal = kind === 'TTY';
    const err = this.open(fd);
    if (err !== 0) reportInitFailure(ctx, err);
  }

  // Taking a descriptor over is where the stream table would own it, so the
  // run's rule stands here too: `open` is reachable on any handle, and on
  // the class itself, without going through the constructor.
  open(fd) {
    if (!native.mayUse(fd)) return uv.UV_EINVAL;
    return super.open(fd);
  }

  // A terminal's mode belongs to the device, so the descriptor the program
  // named reaches it as well as the description this handle reads through.
  // Each switch to raw bases itself on the mode the terminal has then, and a
  // switch back applies that mode, as libuv's handle does; what the run's end
  // hands back is the native's to keep.
  setRawMode(flag) {
    const raw = Boolean(flag);
    if (raw === this._raw) return 0;
    if (raw) {
      const normal = native.getMode(this.fd);
      if (typeof normal === 'number') return normal;
      this._normalMode = normal;
    }
    const err = native.setMode(this.fd, this._normalMode, raw);
    if (err !== 0) return err;
    this._raw = raw;
    return 0;
  }

  getWindowSize(size) {
    return native.getWindowSize(this.fd, size);
  }

  // A terminal was reopened as a description of this process's own, so the
  // one the program named keeps its flags; anything else is carried as it
  // is, and its flag is set the way libuv sets it. Either way a blocking
  // handle's writes complete before they return.
  setBlocking(blocking) {
    this._blocking = Boolean(blocking);
    if (this._terminal) return 0;
    return native.setBlocking(this.fd, this._blocking);
  }

  _writeBytes(req, buffer) {
    if (!this._blocking) return super._writeBytes(req, buffer);
    if (this._closed) return uv.UV_EBADF;
    const written = native.writeSync(this.fd, buffer);
    if (written < 0) return written;
    this.bytesWritten += written;
    streamBaseState[kBytesWritten] = written;
    streamBaseState[kLastWriteWasAsync] = 0;
    return 0;
  }
}

module.exports = { TTY, isTTY };
