'use strict';
// internal/stream_base_commons — the vendored agent installs `onStreamRead`
// as a handle onread hook; this realm's sockets deliver data through JS
// 'data' events instead, so the hook is a recognizable inert marker.
function onStreamRead() {}

module.exports = {
  onStreamRead,
  kUpdateTimer: Symbol('kUpdateTimer'),
  setStreamTimeout(msecs, callback) {
    return this.setTimeout?.(msecs, callback);
  },
};
