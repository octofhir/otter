'use strict';
// internalBinding('fs_event_wrap') — the watcher handle `fs.watch` drives,
// over the host's own change notifications in `internal/otter/fs_watch`.

const native = require('internal/otter/fs_watch');

// Every live handle, by the number the host hands back, so a notification
// arriving from the host thread finds the handle that asked for it.
const handles = new Map();

class FSEvent {
  constructor() {
    this.onchange = null;
    this.initialized = false;
    this._id = 0;
    this._encoding = 'utf8';
  }

  start(path, persistent, recursive, encoding) {
    if (this.initialized) return 0;
    const name = ArrayBuffer.isView(path)
      ? Buffer.from(path.buffer, path.byteOffset, path.byteLength).toString('utf8')
      : String(path);
    const id = native.watch(name, !!recursive, persistent !== false);
    if (id < 0) return id;
    this._id = id;
    this._encoding = encoding || 'utf8';
    this.initialized = true;
    handles.set(id, this);
    return 0;
  }

  close() {
    if (!this.initialized) return;
    this.initialized = false;
    handles.delete(this._id);
    native.close(this._id);
    this._id = 0;
  }

  ref() {
    if (this.initialized) native.hold(this._id, true);
  }

  unref() {
    if (this.initialized) native.hold(this._id, false);
  }

  getAsyncId() { return -1; }
}

// A name reaches JavaScript as text; the caller's encoding decides what it
// is handed as.
function encodeName(name, encoding) {
  if (encoding === 'utf8' || encoding === 'utf-8' || !encoding) return name;
  const bytes = Buffer.from(name, 'utf8');
  if (encoding === 'buffer') return bytes;
  return bytes.toString(encoding);
}

globalThis.__otterFsWatchDeliver = function deliverWatchEvent(id, eventType, filename) {
  const handle = handles.get(id);
  if (handle === undefined || !handle.initialized) return;
  const onchange = handle.onchange;
  if (typeof onchange !== 'function') return;
  onchange.call(handle, 0, eventType, encodeName(filename, handle._encoding));
};
Object.defineProperty(globalThis, '__otterFsWatchDeliver', { enumerable: false });

module.exports = { FSEvent };
