'use strict';

// internal/child_process — what a child's streams were asked to be, checked
// once and named plainly, before anything is started.
//
// A slot can be a word ("pipe", "ignore", "inherit"), a descriptor the caller
// already holds, a stream that stands for one, or the channel a forked child
// speaks over. Everything downstream — the launch itself, and the streams the
// caller reads afterwards — works from the answer this gives, so a wrong
// argument is refused before a process exists to clean up.

const {
  ERR_INVALID_ARG_VALUE,
  ERR_INVALID_SYNC_FORK_INPUT,
  ERR_IPC_ONE_PIPE,
  ERR_IPC_SYNC_FORK,
} = require('internal/errors').codes;
const { isArrayBufferView } = require('internal/util/types');

// What a one-word `stdio` stands for, and where the channel goes when the
// caller did not spell the slots out.
function stdioStringToArray(stdio, channel) {
  let options;
  switch (stdio) {
    case 'ignore':
    case 'overlapped':
    case 'pipe':
      options = [stdio, stdio, stdio];
      break;
    case 'inherit':
      options = [0, 1, 2];
      break;
    default:
      throw new ERR_INVALID_ARG_VALUE('stdio', stdio);
  }
  if (channel) options.push(channel);
  return options;
}

// The descriptor a stream stands for, or -1 when it stands for none.
function descriptorOf(stream) {
  if (typeof stream?.fd === 'number' && stream.fd >= 0) return stream.fd;
  const handle = stream?._handle ?? stream?.handle;
  if (typeof handle?.fd === 'number' && handle.fd >= 0) return handle.fd;
  return -1;
}

function isHandle(stream) {
  const handle = stream?._handle ?? stream?.handle ?? stream;
  return typeof handle === 'object' && handle !== null &&
    typeof handle.readStart === 'function';
}

function getValidStdio(stdio, sync) {
  let ipc;
  let ipcFd;

  if (typeof stdio === 'string') {
    stdio = stdioStringToArray(stdio);
  } else if (!Array.isArray(stdio)) {
    throw new ERR_INVALID_ARG_VALUE('stdio', stdio);
  }

  // A child always has the three streams every process has, whatever the
  // caller said about them.
  while (stdio.length < 3) stdio.push(undefined);

  const parsed = [];
  for (let index = 0; index < stdio.length; index++) {
    let entry = stdio[index];
    entry ??= index < 3 ? 'pipe' : 'ignore';

    if (entry === 'ignore') {
      parsed.push({ type: 'ignore' });
    } else if (entry === 'pipe' || entry === 'overlapped' ||
               (typeof entry === 'number' && entry < 0)) {
      parsed.push({
        type: entry === 'overlapped' ? 'overlapped' : 'pipe',
        readable: index === 0,
        writable: index !== 0,
      });
    } else if (entry === 'ipc') {
      if (sync || ipc !== undefined) {
        if (sync) throw new ERR_IPC_SYNC_FORK();
        throw new ERR_IPC_ONE_PIPE();
      }
      ipc = true;
      ipcFd = index;
      // The channel is not one of the child's streams — it is arranged
      // separately — but it keeps its place, so the slots after it are the
      // numbers the caller meant.
      parsed.push({ type: 'pipe', ipc: true });
    } else if (entry === 'inherit') {
      parsed.push({ type: 'inherit', fd: index });
    } else if (typeof entry === 'number' || typeof entry?.fd === 'number') {
      parsed.push({ type: 'fd', fd: typeof entry === 'number' ? entry : entry.fd });
    } else if (isHandle(entry)) {
      const handle = entry._handle ?? entry.handle ?? entry;
      parsed.push({ type: 'wrap', wrapType: 'pipe', handle, _stdio: entry });
    } else if (isArrayBufferView(entry) || typeof entry === 'string') {
      // A synchronous run takes its input as text; anywhere else it is not
      // something a stream can be made of.
      if (!sync) throw new ERR_INVALID_SYNC_FORK_INPUT(entry);
    } else {
      throw new ERR_INVALID_ARG_VALUE('stdio', entry);
    }
  }

  return { stdio: parsed, ipc, ipcFd };
}

// The channel and the streams belong to the module a program requires; this
// one is asked for them by tests and by Node's own libraries, and answers with
// what is already there rather than with a second implementation.
Object.defineProperty(module.exports, 'ChildProcess', {
  configurable: true,
  enumerable: true,
  get() { return require('child_process').ChildProcess; },
});

module.exports.getValidStdio = getValidStdio;
module.exports.stdioStringToArray = stdioStringToArray;
