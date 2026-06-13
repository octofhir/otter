'use strict';
// `node:fs` — built on the native raw sync core (`__fsnative`). Raw bytes cross
// the boundary as latin1 strings; this layer wraps them in Buffers, applies
// encodings, and adds the Stats/Dirent classes, async callbacks, fs.promises,
// and the file streams.

const native = require('__fsnative');
const { Buffer } = require('buffer');
const { Readable, Writable } = require('stream');

const constants = {
  F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1,
  O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, O_CREAT: 0o100, O_EXCL: 0o200,
  O_NOCTTY: 0o400, O_TRUNC: 0o1000, O_APPEND: 0o2000, O_DIRECTORY: 0o200000,
  O_SYNC: 0o4010000, O_NONBLOCK: 0o4000,
  S_IFMT: 0o170000, S_IFREG: 0o100000, S_IFDIR: 0o040000, S_IFCHR: 0o020000,
  S_IFBLK: 0o060000, S_IFIFO: 0o010000, S_IFLNK: 0o120000, S_IFSOCK: 0o140000,
  S_IRWXU: 0o700, S_IRUSR: 0o400, S_IWUSR: 0o200, S_IXUSR: 0o100,
  COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
  UV_FS_O_FILEMAP: 0,
};

function pathStr(p) {
  if (p instanceof URL) return decodeURIComponent(p.pathname);
  if (Buffer.isBuffer(p)) return p.toString('utf8');
  return String(p);
}

function encodingOf(options) {
  if (!options) return null;
  if (typeof options === 'string') return options;
  return options.encoding || null;
}

class Stats {
  constructor(raw) {
    this.dev = raw.dev; this.ino = raw.ino; this.mode = raw.mode; this.nlink = raw.nlink;
    this.uid = raw.uid; this.gid = raw.gid; this.rdev = raw.rdev; this.size = raw.size;
    this.blksize = raw.blksize; this.blocks = raw.blocks;
    this.atimeMs = raw.atimeMs; this.mtimeMs = raw.mtimeMs;
    this.ctimeMs = raw.ctimeMs; this.birthtimeMs = raw.birthtimeMs;
    this.atime = new Date(raw.atimeMs); this.mtime = new Date(raw.mtimeMs);
    this.ctime = new Date(raw.ctimeMs); this.birthtime = new Date(raw.birthtimeMs);
    this._f = raw.isFile; this._d = raw.isDirectory; this._l = raw.isSymbolicLink;
  }
  isFile() { return this._f; }
  isDirectory() { return this._d; }
  isSymbolicLink() { return this._l; }
  isBlockDevice() { return false; }
  isCharacterDevice() { return false; }
  isFIFO() { return false; }
  isSocket() { return false; }
}

class Dirent {
  constructor(name, row, parentPath) {
    this.name = name;
    this.parentPath = parentPath;
    this.path = parentPath;
    this._d = row.isDir; this._f = row.isFile; this._l = row.isSymlink;
  }
  isFile() { return this._f; }
  isDirectory() { return this._d; }
  isSymbolicLink() { return this._l; }
  isBlockDevice() { return false; }
  isCharacterDevice() { return false; }
  isFIFO() { return false; }
  isSocket() { return false; }
}

// ---- sync ----
function readFileSync(path, options) {
  const raw = native.readRaw(pathStr(path));
  const buf = Buffer.from(raw, 'latin1');
  const enc = encodingOf(options);
  return enc ? buf.toString(enc) : buf;
}
function toLatin1(data, options) {
  if (typeof data === 'string') return Buffer.from(data, encodingOf(options) || 'utf8').toString('latin1');
  if (Buffer.isBuffer(data)) return data.toString('latin1');
  if (data instanceof Uint8Array) return Buffer.from(data).toString('latin1');
  return Buffer.from(String(data), 'utf8').toString('latin1');
}
function writeFileSync(path, data, options) {
  native.writeRaw(pathStr(path), toLatin1(data, options), false);
}
function appendFileSync(path, data, options) {
  native.writeRaw(pathStr(path), toLatin1(data, options), true);
}
function existsSync(path) {
  try { return native.existsRaw(pathStr(path)); } catch { return false; }
}
function statSync(path, options) {
  try { return new Stats(native.statRaw(pathStr(path), false)); }
  catch (e) { if (options && options.throwIfNoEntry === false) return undefined; throw e; }
}
function lstatSync(path, options) {
  try { return new Stats(native.statRaw(pathStr(path), true)); }
  catch (e) { if (options && options.throwIfNoEntry === false) return undefined; throw e; }
}
function readdirSync(path, options) {
  const p = pathStr(path);
  if (options && options.withFileTypes) {
    return native.readdirTypes(p).map((row) => new Dirent(row.name, row, p));
  }
  const names = native.readdirRaw(p);
  const enc = encodingOf(options);
  if (enc === 'buffer') return names.map((n) => Buffer.from(n, 'utf8'));
  return names;
}
function mkdirSync(path, options) {
  const recursive = !!(options && typeof options === 'object' && options.recursive);
  native.mkdir(pathStr(path), recursive);
  return undefined;
}
function rmSync(path, options) {
  native.rm(pathStr(path), !!(options && options.recursive), !!(options && options.force));
}
function rmdirSync(path, options) {
  native.rmdir(pathStr(path), !!(options && options.recursive));
}
function unlinkSync(path) { native.unlink(pathStr(path)); }
function realpathSync(path) { return native.realpath(pathStr(path)); }
realpathSync.native = realpathSync;
function copyFileSync(src, dest) { native.copyFile(pathStr(src), pathStr(dest)); }
function accessSync(path) { native.access(pathStr(path), 0); }
function renameSync(from, to) { native.rename(pathStr(from), pathStr(to)); }
function readlinkSync(path) { return native.readlink(pathStr(path)); }
function chmodSync(path, mode) { native.chmod(pathStr(path), Number(mode)); }
function truncateSync(path, len) { native.truncate(pathStr(path), Number(len) || 0); }

// ---- async (callback) ----
function asyncify(syncFn) {
  return function (...args) {
    const cb = typeof args[args.length - 1] === 'function' ? args.pop() : () => {};
    setTimeout(() => {
      let result;
      try { result = syncFn(...args); } catch (err) { return cb(err); }
      cb(null, result);
    });
  };
}

const readFile = asyncify(readFileSync);
const writeFile = asyncify(writeFileSync);
const appendFile = asyncify(appendFileSync);
const stat = asyncify(statSync);
const lstat = asyncify(lstatSync);
const readdir = asyncify(readdirSync);
const mkdir = asyncify(mkdirSync);
const rm = asyncify(rmSync);
const rmdir = asyncify(rmdirSync);
const unlink = asyncify(unlinkSync);
const realpath = asyncify(realpathSync);
const copyFile = asyncify(copyFileSync);
const access = asyncify(accessSync);
const rename = asyncify(renameSync);
const readlink = asyncify(readlinkSync);
const chmod = asyncify(chmodSync);
const truncate = asyncify(truncateSync);
function exists(path, cb) { setTimeout(() => cb(existsSync(path))); }

// ---- promises ----
function promisify(syncFn) {
  return function (...args) {
    return new Promise((resolve, reject) => {
      setTimeout(() => {
        try { resolve(syncFn(...args)); } catch (err) { reject(err); }
      });
    });
  };
}
const promises = {
  readFile: promisify(readFileSync),
  writeFile: promisify(writeFileSync),
  appendFile: promisify(appendFileSync),
  stat: promisify(statSync),
  lstat: promisify(lstatSync),
  readdir: promisify(readdirSync),
  mkdir: promisify(mkdirSync),
  rm: promisify(rmSync),
  rmdir: promisify(rmdirSync),
  unlink: promisify(unlinkSync),
  realpath: promisify(realpathSync),
  copyFile: promisify(copyFileSync),
  access: promisify(accessSync),
  rename: promisify(renameSync),
  readlink: promisify(readlinkSync),
  chmod: promisify(chmodSync),
  truncate: promisify(truncateSync),
  constants,
};

// ---- streams ----
class ReadStream extends Readable {
  constructor(path, options = {}) {
    super(typeof options === 'object' ? options : {});
    this.path = pathStr(path);
    this.bytesRead = 0;
    const enc = encodingOf(options);
    setTimeout(() => {
      try {
        const buf = readFileSync(this.path);
        this.emit('open', 0);
        this.emit('ready');
        const start = options.start || 0;
        const end = options.end !== undefined ? options.end + 1 : buf.length;
        const slice = buf.slice(start, end);
        this.bytesRead = slice.length;
        this.push(enc ? slice.toString(enc) : slice);
        this.push(null);
        this.emit('close');
      } catch (err) { this.destroy(err); }
    });
  }
  close(cb) { if (cb) setTimeout(cb); }
}
class WriteStream extends Writable {
  constructor(path, options = {}) {
    super(typeof options === 'object' ? options : {});
    this.path = pathStr(path);
    this.bytesWritten = 0;
    this._chunks = [];
    const flags = (options && options.flags) || 'w';
    this._append = flags.includes('a');
    setTimeout(() => { this.emit('open', 0); this.emit('ready'); });
  }
  _write(chunk, encoding, cb) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), encoding || 'utf8');
    this._chunks.push(buf);
    this.bytesWritten += buf.length;
    cb();
  }
  _final(cb) {
    try {
      const all = Buffer.concat(this._chunks);
      if (this._append) appendFileSync(this.path, all);
      else writeFileSync(this.path, all);
      this.emit('close');
      cb();
    } catch (err) { cb(err); }
  }
  close(cb) { if (cb) this.once('close', cb); this.end(); }
}
function createReadStream(path, options) { return new ReadStream(path, options); }
function createWriteStream(path, options) { return new WriteStream(path, options); }

// ---- watch stubs ----
function watch() {
  const w = { close() {}, ref() { return w; }, unref() { return w; }, on() { return w; }, once() { return w; }, removeListener() { return w; } };
  return w;
}
function watchFile() { return { stop() {} }; }
function unwatchFile() {}

module.exports = {
  constants, Stats, Dirent, ReadStream, WriteStream,
  readFileSync, writeFileSync, appendFileSync, existsSync, statSync, lstatSync,
  readdirSync, mkdirSync, rmSync, rmdirSync, unlinkSync, realpathSync,
  copyFileSync, accessSync, renameSync, readlinkSync, chmodSync, truncateSync,
  readFile, writeFile, appendFile, exists, stat, lstat, readdir, mkdir, rm, rmdir,
  unlink, realpath, copyFile, access, rename, readlink, chmod, truncate,
  createReadStream, createWriteStream, watch, watchFile, unwatchFile,
  promises,
};
