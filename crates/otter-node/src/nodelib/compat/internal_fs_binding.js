'use strict';

// internalBinding('fs') — the contract vendored `fs.js` and `internal/fs/*`
// drive, over the host file surface in `internal/otter/fs`.
//
// Every call takes an optional request object as its last argument: with
// one the work reports through `req.oncomplete(err, result)`, without one
// it answers directly and throws. The host surface is synchronous, so an
// asynchronous call runs the work and delivers its result on the next
// turn, which keeps the ordering a caller sees.

const native = require('internal/otter/fs');

// Stats arrive as the 18 numbers `getStatsFromBinding` reads.
const kStatsFields = 18;

class FSReqCallback {
  constructor(bigint = false) {
    this.oncomplete = null;
    this.context = null;
    this.bigint = bigint;
  }
}

function statsFrom(values, bigint) {
  const view = bigint ? new BigInt64Array(kStatsFields) : new Float64Array(kStatsFields);
  for (let i = 0; i < kStatsFields; i++) {
    view[i] = bigint ? BigInt(Math.trunc(values[i])) : values[i];
  }
  return view;
}

// `statSync` and friends answer nothing rather than throwing when the
// caller passed `throwIfNoEntry: false` and the path is simply absent.
// Every other failure is still a failure.
function statOrNothing(work) {
  try {
    return work();
  } catch (error) {
    if (error.code === 'ENOENT') return undefined;
    throw error;
  }
}

// The promise API asks for its result by passing this in place of a
// request object.
const kUsePromises = Symbol('kUsePromises');

// Run `work` for a call that is synchronous, callback-driven, or promised.
function dispatch(req, work) {
  if (req === undefined || req === null) return work();
  if (req === kUsePromises) {
    return new Promise((resolve, reject) => {
      setImmediate(() => {
        try {
          resolve(work());
        } catch (error) {
          reject(error);
        }
      });
    });
  }
  setImmediate(() => {
    let result;
    try {
      result = work();
    } catch (error) {
      req.oncomplete(error);
      return;
    }
    // A call that answers nothing hands its callback the failure slot
    // alone, and that slot is `null`, not `undefined`.
    if (result === undefined) {
      req.oncomplete(null);
    } else {
      req.oncomplete(null, result);
    }
  });
  return undefined;
}

// The handle `fs.promises` wraps: a descriptor plus the two closes its
// `FileHandle` calls directly.
class FileHandleBinding {
  constructor(fd) {
    this.fd = fd;
  }

  close() {
    return dispatch(kUsePromises, () => native.close(this.fd));
  }

  closeSync() {
    native.close(this.fd);
  }

  getAsyncId() { return -1; }

  release() {}
}


// A descriptor argument is validated here because this is where Node
// validates it: `fs.closeSync(null)` must be a `TypeError` before anything
// touches the descriptor table.
const { validateInt32 } = require('internal/validators');

function validatedFd(fd) {
  if (Object.is(fd, -0)) return 0;
  validateInt32(fd, 'fd', 0);
  return fd;
}

// A path reaches the binding as text or as the bytes of a name that is not
// valid UTF-8. Either way the host wants the text.
function pathText(value) {
  if (Buffer.isBuffer(value)) return value.toString('utf8');
  if (ArrayBuffer.isView(value)) {
    return Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString('utf8');
  }
  return String(value);
}

// A name the host reports is text; the encoding the caller asked for
// decides what it is handed as. `buffer` asks for the bytes themselves.
function encodeName(name, encoding) {
  if (encoding === undefined || encoding === null ||
      encoding === 'utf8' || encoding === 'utf-8') {
    return name;
  }
  const bytes = Buffer.from(name, 'utf8');
  return encoding === 'buffer' ? bytes : bytes.toString(encoding);
}

function encodeNames(names, encoding) {
  if (encoding === undefined || encoding === null ||
      encoding === 'utf8' || encoding === 'utf-8') {
    return names;
  }
  const out = new Array(names.length);
  for (let i = 0; i < names.length; i++) out[i] = encodeName(names[i], encoding);
  return out;
}

// `fs.watchFile` polls, and the host does the polling: the handle only
// carries the result of one poll back to its owner.
const watchNative = require('internal/otter/fs_watch');
const statWatchers = new Map();
const kUseBigintSlot = Symbol('kUseBigint');

class StatWatcher {
  constructor(useBigint = false) {
    this.onchange = null;
    this[kUseBigintSlot] = !!useBigint;
    this._id = 0;
  }

  start(path, interval) {
    if (this._id !== 0) return 0;
    const id = watchNative.watchFile(pathText(path), interval, true);
    if (id < 0) return id;
    this._id = id;
    statWatchers.set(id, this);
    return 0;
  }

  close() {
    if (this._id === 0) return;
    statWatchers.delete(this._id);
    watchNative.close(this._id);
    this._id = 0;
  }

  ref() {
    if (this._id !== 0) watchNative.hold(this._id, true);
  }

  unref() {
    if (this._id !== 0) watchNative.hold(this._id, false);
  }

  getAsyncId() { return -1; }
}

globalThis.__otterFsWatchFileDeliver = function deliverPoll(id, status, slots) {
  const watcher = statWatchers.get(id);
  if (watcher === undefined) return;
  const onchange = watcher.onchange;
  if (typeof onchange !== 'function') return;
  const bigint = watcher[kUseBigintSlot];
  const view = bigint ? new BigInt64Array(slots.length) : new Float64Array(slots.length);
  for (let i = 0; i < slots.length; i++) {
    view[i] = bigint ? BigInt(Math.trunc(slots[i])) : slots[i];
  }
  onchange.call(watcher, status, view);
};
Object.defineProperty(globalThis, '__otterFsWatchFileDeliver', { enumerable: false });

module.exports = {
  FSReqCallback,
  StatWatcher,
  kUsePromises,
  kFsStatsFieldsNumber: kStatsFields,

  openFileHandle(path, flags, mode, req) {
    return dispatch(req, () => new FileHandleBinding(native.open(pathText(path), flags, mode)));
  },

  open(path, flags, mode, req) {
    return dispatch(req, () => native.open(pathText(path), flags, mode));
  },
  close(fd, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.close(handle));
  },
  read(fd, buffer, offset, length, position, req) {
    return dispatch(req, () => native.read(fd, buffer, offset, length, position ?? -1));
  },
  readBuffers(fd, buffers, position, req) {
    return dispatch(req, () => {
      let total = 0;
      for (const buffer of buffers) {
        const at = position === null || position === undefined || position < 0
          ? -1
          : position + total;
        total += native.read(fd, buffer, 0, buffer.byteLength, at);
      }
      return total;
    });
  },
  writeBuffer(fd, buffer, offset, length, position, req) {
    return dispatch(req, () => native.writeBuffer(fd, buffer, offset, length, position ?? -1));
  },
  writeBuffers(fd, buffers, position, req) {
    return dispatch(req, () => {
      let total = 0;
      for (const buffer of buffers) {
        const at = position === null || position === undefined || position < 0
          ? -1
          : position + total;
        total += native.writeBuffer(fd, buffer, 0, buffer.byteLength, at);
      }
      return total;
    });
  },
  writeString(fd, value, position, encoding, req) {
    return dispatch(req, () => native.writeString(fd, value, position ?? -1, encoding));
  },
  fstat(fd, useBigint, req, doNotThrow) {
    if (doNotThrow) {
      try {
        return statsFrom(native.fstat(fd), useBigint);
      } catch {
        return undefined;
      }
    }
    return dispatch(req, () => statsFrom(native.fstat(fd), useBigint));
  },
  stat(path, useBigint, req, throwIfNoEntry) {
    if (req === undefined && throwIfNoEntry === false) {
      return statOrNothing(() => statsFrom(native.stat(pathText(path)), useBigint));
    }
    return dispatch(req, () => statsFrom(native.stat(pathText(path)), useBigint));
  },
  lstat(path, useBigint, req, throwIfNoEntry) {
    if (req === undefined && throwIfNoEntry === false) {
      return statOrNothing(() => statsFrom(native.lstat(pathText(path)), useBigint));
    }
    return dispatch(req, () => statsFrom(native.lstat(pathText(path)), useBigint));
  },
  statfs(path, useBigint, req) {
    return dispatch(req, () => {
      const values = native.statfs(pathText(path));
      return useBigint ? values.map((value) => BigInt(Math.trunc(value))) : values;
    });
  },
  access(path, mode, req) {
    return dispatch(req, () => native.access(pathText(path), mode));
  },
  chmod(path, mode, req) {
    return dispatch(req, () => native.chmod(pathText(path), mode));
  },
  fchmod(fd, mode, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.fchmod(handle, mode));
  },
  chown(path, uid, gid, req) {
    return dispatch(req, () => native.chown(pathText(path), uid, gid));
  },
  lchown(path, uid, gid, req) {
    return dispatch(req, () => native.lchown(pathText(path), uid, gid));
  },
  fchown(fd, uid, gid, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.fchown(handle, uid, gid));
  },
  copyFile(source, target, mode, req) {
    return dispatch(req, () => native.copyFile(pathText(source), pathText(target), mode));
  },
  rename(from, to, req) {
    return dispatch(req, () => native.rename(pathText(from), pathText(to)));
  },
  unlink(path, req) {
    return dispatch(req, () => native.unlink(pathText(path)));
  },
  rmdir(path, req) {
    return dispatch(req, () => native.rmdir(pathText(path)));
  },
  rmSync(path, maxRetries, recursive, retryDelay) {
    return native.rmSync(pathText(path), maxRetries, recursive, retryDelay);
  },
  mkdir(path, mode, recursive, req) {
    return dispatch(req, () => native.mkdir(pathText(path), mode, recursive));
  },
  mkdtemp(prefix, encoding, req) {
    return dispatch(req, () => encodeName(native.mkdtemp(pathText(prefix), encoding), encoding));
  },
  readdir(path, encoding, withFileTypes, req) {
    return dispatch(req, () => {
      const result = native.readdir(pathText(path), encoding, withFileTypes);
      if (withFileTypes) {
        return [encodeNames(result[0], encoding), result[1]];
      }
      return encodeNames(result, encoding);
    });
  },
  readlink(path, encoding, req) {
    return dispatch(req, () => encodeName(native.readlink(pathText(path), encoding), encoding));
  },
  realpath(path, encoding, req) {
    return dispatch(req, () => encodeName(native.realpath(pathText(path), encoding), encoding));
  },
  symlink(target, path, type, req) {
    return dispatch(req, () => native.symlink(pathText(target), pathText(path), type));
  },
  link(existing, path, req) {
    return dispatch(req, () => native.link(pathText(existing), pathText(path)));
  },
  ftruncate(fd, length, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.ftruncate(handle, length));
  },
  fsync(fd, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.fsync(handle));
  },
  fdatasync(fd, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.fdatasync(handle));
  },
  utimes(path, atime, mtime, req) {
    return dispatch(req, () => native.utimes(pathText(path), atime, mtime));
  },
  lutimes(path, atime, mtime, req) {
    return dispatch(req, () => native.lutimes(pathText(path), atime, mtime));
  },
  futimes(fd, atime, mtime, req) {
    const handle = validatedFd(fd);
    return dispatch(req, () => native.futimes(handle, atime, mtime));
  },
  existsSync(path) {
    return native.existsSync(pathText(path));
  },
  internalModuleStat(path) {
    return native.internalModuleStat(pathText(path));
  },
  cpSyncCheckPaths(src, dest, dereference, recursive) {
    return native.cpSyncCheckPaths(pathText(src), pathText(dest), !!dereference, !!recursive);
  },
  cpSyncOverrideFile(src, dest, mode, preserveTimestamps) {
    return native.cpSyncOverrideFile(pathText(src), pathText(dest), mode | 0, !!preserveTimestamps);
  },
  cpSyncCopyDir(src, dest, force, dereference, errorOnExist, verbatimSymlinks, preserveTimestamps) {
    return native.cpSyncCopyDir(pathText(src), pathText(dest), !!force, !!dereference,
                                !!errorOnExist, !!verbatimSymlinks, !!preserveTimestamps);
  },
  // Both of these take a path or an already-open descriptor. A descriptor
  // is read and written where it stands and is left open; only the path
  // form opens and closes a file of its own.
  readFileUtf8(path, flags) {
    if (typeof path === 'number') {
      const chunks = [];
      const chunk = Buffer.allocUnsafe(65536);
      let read;
      while ((read = native.read(validatedFd(path), chunk, 0, chunk.byteLength, -1)) > 0) {
        chunks.push(Buffer.from(chunk.subarray(0, read)));
      }
      return Buffer.concat(chunks).toString('utf8');
    }
    return native.readFileUtf8(pathText(path), flags);
  },
  writeFileUtf8(path, data, flags, mode) {
    if (typeof path === 'number') {
      const fd = validatedFd(path);
      const bytes = Buffer.from(data, 'utf8');
      let written = 0;
      while (written < bytes.byteLength) {
        written += native.writeBuffer(fd, bytes, written, bytes.byteLength - written, -1);
      }
      return undefined;
    }
    return native.writeFileUtf8(pathText(path), data, flags, mode);
  },
};
