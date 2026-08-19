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
    req.oncomplete(undefined, result);
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

module.exports = {
  FSReqCallback,
  kUsePromises,

  openFileHandle(path, flags, mode, req) {
    return dispatch(req, () => new FileHandleBinding(native.open(`${path}`, flags, mode)));
  },

  open(path, flags, mode, req) {
    return dispatch(req, () => native.open(`${path}`, flags, mode));
  },
  close(fd, req) {
    return dispatch(req, () => native.close(fd));
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
  fstat(fd, useBigint, req, shouldNotThrow) {
    if (shouldNotThrow) {
      try {
        return statsFrom(native.fstat(fd), useBigint);
      } catch {
        return undefined;
      }
    }
    return dispatch(req, () => statsFrom(native.fstat(fd), useBigint));
  },
  stat(path, useBigint, req, shouldNotThrow) {
    if (shouldNotThrow) {
      try {
        return statsFrom(native.stat(`${path}`), useBigint);
      } catch {
        return undefined;
      }
    }
    return dispatch(req, () => statsFrom(native.stat(`${path}`), useBigint));
  },
  lstat(path, useBigint, req, shouldNotThrow) {
    if (shouldNotThrow) {
      try {
        return statsFrom(native.lstat(`${path}`), useBigint);
      } catch {
        return undefined;
      }
    }
    return dispatch(req, () => statsFrom(native.lstat(`${path}`), useBigint));
  },
  statfs(path, useBigint, req) {
    return dispatch(req, () => {
      const values = native.statfs(`${path}`);
      return useBigint ? values.map((value) => BigInt(Math.trunc(value))) : values;
    });
  },
  access(path, mode, req) {
    return dispatch(req, () => native.access(`${path}`, mode));
  },
  chmod(path, mode, req) {
    return dispatch(req, () => native.chmod(`${path}`, mode));
  },
  fchmod(fd, mode, req) {
    return dispatch(req, () => native.fchmod(fd, mode));
  },
  chown(path, uid, gid, req) {
    return dispatch(req, () => native.chown(`${path}`, uid, gid));
  },
  lchown(path, uid, gid, req) {
    return dispatch(req, () => native.lchown(`${path}`, uid, gid));
  },
  fchown(fd, uid, gid, req) {
    return dispatch(req, () => native.fchown(fd, uid, gid));
  },
  copyFile(source, target, mode, req) {
    return dispatch(req, () => native.copyFile(`${source}`, `${target}`, mode));
  },
  rename(from, to, req) {
    return dispatch(req, () => native.rename(`${from}`, `${to}`));
  },
  unlink(path, req) {
    return dispatch(req, () => native.unlink(`${path}`));
  },
  rmdir(path, req) {
    return dispatch(req, () => native.rmdir(`${path}`));
  },
  rmSync(path, maxRetries, recursive, retryDelay) {
    return native.rmSync(`${path}`, maxRetries, recursive, retryDelay);
  },
  mkdir(path, mode, recursive, req) {
    return dispatch(req, () => native.mkdir(`${path}`, mode, recursive));
  },
  mkdtemp(prefix, encoding, req) {
    return dispatch(req, () => native.mkdtemp(`${prefix}`, encoding));
  },
  readdir(path, encoding, withFileTypes, req) {
    return dispatch(req, () => native.readdir(`${path}`, encoding, withFileTypes));
  },
  readlink(path, encoding, req) {
    return dispatch(req, () => native.readlink(`${path}`, encoding));
  },
  realpath(path, encoding, req) {
    return dispatch(req, () => native.realpath(`${path}`, encoding));
  },
  symlink(target, path, type, req) {
    return dispatch(req, () => native.symlink(`${target}`, `${path}`, type));
  },
  link(existing, path, req) {
    return dispatch(req, () => native.link(`${existing}`, `${path}`));
  },
  ftruncate(fd, length, req) {
    return dispatch(req, () => native.ftruncate(fd, length));
  },
  fsync(fd, req) {
    return dispatch(req, () => native.fsync(fd));
  },
  fdatasync(fd, req) {
    return dispatch(req, () => native.fdatasync(fd));
  },
  utimes(path, atime, mtime, req) {
    return dispatch(req, () => native.utimes(`${path}`, atime, mtime));
  },
  lutimes(path, atime, mtime, req) {
    return dispatch(req, () => native.lutimes(`${path}`, atime, mtime));
  },
  futimes(fd, atime, mtime, req) {
    return dispatch(req, () => native.futimes(fd, atime, mtime));
  },
  existsSync(path) {
    return native.existsSync(`${path}`);
  },
  internalModuleStat(receiver, path) {
    return native.internalModuleStat(receiver, `${path}`);
  },
  readFileUtf8(path, flags) {
    return native.readFileUtf8(`${path}`, flags);
  },
  writeFileUtf8(path, data, flags, mode) {
    return native.writeFileUtf8(`${path}`, data, flags, mode);
  },
};
