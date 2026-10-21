'use strict';

// internalBinding('tty_wrap') — terminal handles over the host stream table.
//
// A `TTY` takes its descriptor over when it is made, the way libuv's
// `uv_tty_init` does: a terminal is reopened as a description of this
// process's own, a pipe or a socket is carried as it is, and what a poller
// cannot wait on — a regular file, a device that is not a terminal, a
// descriptor that is not open — is refused with EINVAL through `ctx`, which
// is how `tty.ReadStream` and `tty.WriteStream` learn of it. Beyond the
// stream it carries what only a terminal has: raw mode, the window size, and
// writes that complete before they return.
//
// A terminal switched to raw mode is the run's to hand back. The first
// switch on a descriptor records the mode it replaced, here, in this
// isolate's own module state; the runtime calls the reset this module leaves
// on `process` once the run is over, and before `abort()`, so a program that
// exits while raw does not leave its shell raw.

const { internalBinding, primordials } = require('internal/bootstrap/realm');
const { ObjectDefineProperty } = primordials;
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

// The terminals this run switched to raw mode, each with the mode it had
// before its first switch, in the order they were switched.
const switchedTerminals = [];

function rememberNormalMode(fd, mode) {
  for (let i = 0; i < switchedTerminals.length; i++) {
    if (switchedTerminals[i].fd === fd) return;
  }
  switchedTerminals[switchedTerminals.length] = { __proto__: null, fd, mode };
  if (switchedTerminals.length === 1) {
    ObjectDefineProperty(process, '__otter_reset_terminals__', {
      __proto__: null,
      value: resetTerminals,
      configurable: true,
    });
  }
}

// Hand every switched terminal its mode back, unless it already has it.
function resetTerminals() {
  for (let i = 0; i < switchedTerminals.length; i++) {
    const { fd, mode } = switchedTerminals[i];
    native.resetMode(fd, mode);
  }
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
    const kind = guessHandleType(fd);
    if (kind === 'FILE' || kind === 'UNKNOWN') {
      reportInitFailure(ctx, uv.UV_EINVAL);
      return;
    }
    this._terminal = kind === 'TTY';
    const err = this.open(fd);
    if (err !== 0) reportInitFailure(ctx, err);
  }

  // A terminal's mode belongs to the device, so the descriptor the program
  // named reaches it as well as the description this handle reads through.
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
    if (raw) rememberNormalMode(this.fd, this._normalMode);
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
