'use strict';
// internalBinding('fs_event_wrap') — the watcher handle `fs.watch` drives.
// This runtime has no change notifications from the host yet, so a handle
// starts, holds nothing, and stops; `fs.watchFile` polls instead and works
// through `stat`.
class FSEvent {
  constructor() {
    this.onchange = null;
    this.initialized = false;
  }

  start(_path, _persistent, _recursive, _encoding) {
    this.initialized = true;
    // The host reports no events, so nothing is ever delivered.
    return 0;
  }

  close() {
    this.initialized = false;
  }

  ref() {}
  unref() {}
  getAsyncId() { return -1; }
}

module.exports = { FSEvent };
