'use strict';

// internalBinding('fs_dir') — the directory handle `internal/fs/dir`
// drives. A handle reads its directory once and hands the entries back in
// batches of the size the caller asks for, as name/type pairs; `null` ends
// the walk.

const binding = require('internal/otter/fs_binding');

class DirHandle {
  constructor(path, encoding) {
    const [names, types] = binding.readdir(path, encoding, true);
    this._entries = [];
    for (let i = 0; i < names.length; i++) this._entries.push(names[i], types[i]);
    this._offset = 0;
    this._closed = false;
  }

  read(encoding, bufferSize, req) {
    const work = () => {
      if (this._closed || this._offset >= this._entries.length) return null;
      const take = Math.max(1, (bufferSize | 0) || 32) * 2;
      const batch = this._entries.slice(this._offset, this._offset + take);
      this._offset += batch.length;
      return batch;
    };
    if (req === undefined || req === null) return work();
    setImmediate(() => {
      let result;
      try {
        result = work();
      } catch (error) {
        req.oncomplete(error);
        return;
      }
      req.oncomplete(undefined, result);
    });
    return undefined;
  }

  close(req) {
    this._closed = true;
    if (req !== undefined && req !== null) setImmediate(() => req.oncomplete(undefined));
    return undefined;
  }
}

module.exports = {
  opendir(path, encoding, req) {
    if (req === undefined || req === null) return new DirHandle(`${path}`, encoding);
    setImmediate(() => {
      let handle;
      try {
        handle = new DirHandle(`${path}`, encoding);
      } catch (error) {
        req.oncomplete(error);
        return;
      }
      req.oncomplete(undefined, handle);
    });
    return undefined;
  },
  opendirSync(path, encoding) {
    return new DirHandle(`${path}`, encoding);
  },
};
