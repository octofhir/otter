'use strict';

// internalBinding('stream_wrap') — the request classes and the shared
// stream-state words that vendored `internal/stream_base_commons` reads
// after every stream call. The wraps in `internal/otter/stream_handle`
// fill `streamBaseState` before invoking `onread`/`oncomplete`, exactly
// like the StreamBase C++ side does.

const kReadBytesOrError = 0;
const kArrayBufferOffset = 1;
const kBytesWritten = 2;
const kLastWriteWasAsync = 3;
const streamBaseState = new Int32Array(4);

class WriteWrap {
  getAsyncId() { return -1; }
}

class ShutdownWrap {
  getAsyncId() { return -1; }
}

// Placeholder identity for `internal/js_stream_socket`; instances are not
// functional yet, but the class must exist for instanceof branding.
class JSStream {}

module.exports = {
  WriteWrap,
  ShutdownWrap,
  JSStream,
  streamBaseState,
  kReadBytesOrError,
  kArrayBufferOffset,
  kBytesWritten,
  kLastWriteWasAsync,
};
