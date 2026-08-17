'use strict';
// `node:http` — HTTP/1.1 over `node:net`.
//
// The connection is a socket like any other; everything here is the framing on
// top of it. One incremental parser serves both directions, since a request
// and a response differ only in their first line, and it is fed whatever
// arrives rather than assuming a message shows up in one piece.

const EventEmitter = require('events');
const net = require('net');
const { Buffer } = require('buffer');
const { Readable, Writable } = require('stream');

const STATUS_CODES = {
  100: 'Continue', 101: 'Switching Protocols', 102: 'Processing', 103: 'Early Hints',
  200: 'OK', 201: 'Created', 202: 'Accepted', 203: 'Non-Authoritative Information',
  204: 'No Content', 205: 'Reset Content', 206: 'Partial Content', 207: 'Multi-Status',
  208: 'Already Reported', 226: 'IM Used',
  300: 'Multiple Choices', 301: 'Moved Permanently', 302: 'Found', 303: 'See Other',
  304: 'Not Modified', 305: 'Use Proxy', 307: 'Temporary Redirect', 308: 'Permanent Redirect',
  400: 'Bad Request', 401: 'Unauthorized', 402: 'Payment Required', 403: 'Forbidden',
  404: 'Not Found', 405: 'Method Not Allowed', 406: 'Not Acceptable',
  407: 'Proxy Authentication Required', 408: 'Request Timeout', 409: 'Conflict',
  410: 'Gone', 411: 'Length Required', 412: 'Precondition Failed', 413: 'Payload Too Large',
  414: 'URI Too Long', 415: 'Unsupported Media Type', 416: 'Range Not Satisfiable',
  417: 'Expectation Failed', 418: "I'm a Teapot", 421: 'Misdirected Request',
  422: 'Unprocessable Entity', 423: 'Locked', 424: 'Failed Dependency', 425: 'Too Early',
  426: 'Upgrade Required', 428: 'Precondition Required', 429: 'Too Many Requests',
  431: 'Request Header Fields Too Large', 451: 'Unavailable For Legal Reasons',
  500: 'Internal Server Error', 501: 'Not Implemented', 502: 'Bad Gateway',
  503: 'Service Unavailable', 504: 'Gateway Timeout', 505: 'HTTP Version Not Supported',
  506: 'Variant Also Negotiates', 507: 'Insufficient Storage', 508: 'Loop Detected',
  509: 'Bandwidth Limit Exceeded', 510: 'Not Extended', 511: 'Network Authentication Required',
};

const METHODS = [
  'ACL', 'BIND', 'CHECKOUT', 'CONNECT', 'COPY', 'DELETE', 'GET', 'HEAD', 'LINK', 'LOCK',
  'M-SEARCH', 'MERGE', 'MKACTIVITY', 'MKCALENDAR', 'MKCOL', 'MOVE', 'NOTIFY', 'OPTIONS',
  'PATCH', 'POST', 'PROPFIND', 'PROPPATCH', 'PURGE', 'PUT', 'REBIND', 'REPORT', 'SEARCH',
  'SOURCE', 'SUBSCRIBE', 'TRACE', 'UNBIND', 'UNLINK', 'UNLOCK', 'UNSUBSCRIBE',
];

// Headers whose repeats are joined rather than kept apart, and those that are
// never joined at all. `set-cookie` is the reason the second list exists.
const SINGLE_VALUE = new Set([
  'content-type', 'content-length', 'user-agent', 'referer', 'host', 'authorization',
  'proxy-authorization', 'if-modified-since', 'if-unmodified-since', 'from', 'location',
  'max-forwards', 'retry-after', 'etag', 'last-modified', 'server', 'age', 'expires',
]);

function codedError(message, code) {
  const err = new Error(message);
  err.code = code;
  return err;
}

// ---------------------------------------------------------------- parser ---

// Feeds on whatever the socket produced so far and reports whole messages. The
// caller says which kind of first line to expect, because that is the only
// place a request and a response differ.
class Parser {
  constructor(kind, handlers) {
    this.kind = kind;
    this.handlers = handlers;
    this.buffer = Buffer.alloc(0);
    this.state = 'head';
    this.remaining = 0;
    this.chunkedState = 'size';
  }

  execute(chunk) {
    if (this.state === 'upgraded') return;
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    for (;;) {
      if (this.state === 'head') {
        const end = this.buffer.indexOf('\r\n\r\n');
        if (end === -1) return;
        const head = this.buffer.subarray(0, end).toString('latin1');
        this.buffer = this.buffer.subarray(end + 4);
        if (!this._head(head)) return;
        continue;
      }
      if (this.state === 'length') {
        if (this.buffer.length === 0) return;
        const take = Math.min(this.remaining, this.buffer.length);
        this.handlers.body(this.buffer.subarray(0, take));
        this.buffer = this.buffer.subarray(take);
        this.remaining -= take;
        if (this.remaining === 0) this._complete();
        continue;
      }
      if (this.state === 'chunked') {
        if (!this._chunked()) return;
        continue;
      }
      return;
    }
  }

  _head(head) {
    const lines = head.split('\r\n');
    const first = lines.shift();
    const headers = [];
    for (const line of lines) {
      const split = line.indexOf(':');
      if (split === -1) continue;
      headers.push([line.slice(0, split).trim(), line.slice(split + 1).trim()]);
    }
    const message = this.kind === 'request'
      ? this._requestLine(first)
      : this._statusLine(first);
    if (message === null) {
      this.handlers.error(codedError('Parse Error', 'HPE_INVALID_CONSTANT'));
      return false;
    }
    message.rawHeaders = headers.flat();
    message.headers = collectHeaders(headers);

    // An upgraded connection stops being HTTP: hand the head and whatever
    // followed it to the owner and never parse again.
    const upgraded = message.statusCode === 101 ||
      (typeof message.headers.upgrade === 'string' &&
       /\bupgrade\b/i.test(String(message.headers.connection ?? '')));
    if (upgraded && typeof this.handlers.upgrade === 'function') {
      this.state = 'upgraded';
      const head = this.buffer;
      this.buffer = Buffer.alloc(0);
      this.handlers.upgrade(message, head);
      return false;
    }
    // 1xx responses (other than a 101 upgrade) are informational: report
    // them and keep waiting for the real response on the same connection.
    if (this.kind === 'response' &&
        message.statusCode >= 100 && message.statusCode < 200) {
      if (typeof this.handlers.info === 'function') this.handlers.info(message);
      this.state = 'head';
      return true;
    }
    const framing = this._framing(message);
    this.handlers.head(message);
    if (framing === 'none') {
      this._complete();
    } else if (framing === 'chunked') {
      this.state = 'chunked';
      this.chunkedState = 'size';
    } else {
      this.state = 'length';
      if (this.remaining === 0) this._complete();
    }
    return true;
  }

  // How long the body is, said three different ways: a length, chunks, or
  // nothing at all.
  _framing(message) {
    const encoding = message.headers['transfer-encoding'];
    if (typeof encoding === 'string' && encoding.toLowerCase().includes('chunked')) {
      return 'chunked';
    }
    const length = message.headers['content-length'];
    if (length !== undefined) {
      this.remaining = Number.parseInt(length, 10) || 0;
      return 'length';
    }
    if (this.kind === 'response' && this.handlers.bodyless !== true) {
      // A response with neither framing runs to the end of the connection.
      if (message.statusCode === 204 || message.statusCode === 304 ||
          (message.statusCode >= 100 && message.statusCode < 200)) {
        return 'none';
      }
      this.remaining = Infinity;
      return 'length';
    }
    return 'none';
  }

  _chunked() {
    if (this.chunkedState === 'size') {
      const end = this.buffer.indexOf('\r\n');
      if (end === -1) return false;
      const line = this.buffer.subarray(0, end).toString('latin1');
      this.buffer = this.buffer.subarray(end + 2);
      const size = Number.parseInt(line.split(';')[0].trim(), 16);
      if (Number.isNaN(size)) {
        this.handlers.error(codedError('Parse Error', 'HPE_INVALID_CHUNK_SIZE'));
        return false;
      }
      this.remaining = size;
      this.chunkedState = size === 0 ? 'trailer' : 'data';
      return true;
    }
    if (this.chunkedState === 'data') {
      if (this.buffer.length === 0) return false;
      const take = Math.min(this.remaining, this.buffer.length);
      this.handlers.body(this.buffer.subarray(0, take));
      this.buffer = this.buffer.subarray(take);
      this.remaining -= take;
      if (this.remaining === 0) this.chunkedState = 'crlf';
      return true;
    }
    if (this.chunkedState === 'crlf') {
      if (this.buffer.length < 2) return false;
      this.buffer = this.buffer.subarray(2);
      this.chunkedState = 'size';
      return true;
    }
    // Trailers run to the blank line that ends them.
    const end = this.buffer.indexOf('\r\n');
    if (end === -1) return false;
    if (end === 0) {
      this.buffer = this.buffer.subarray(2);
      this._complete();
      return true;
    }
    this.buffer = this.buffer.subarray(end + 2);
    return true;
  }

  _requestLine(line) {
    const parts = line.split(' ');
    if (parts.length < 3) return null;
    const version = /^HTTP\/(\d)\.(\d)$/.exec(parts[parts.length - 1]);
    if (version === null) return null;
    return {
      method: parts[0],
      url: parts.slice(1, -1).join(' '),
      httpVersionMajor: Number(version[1]),
      httpVersionMinor: Number(version[2]),
      httpVersion: `${version[1]}.${version[2]}`,
    };
  }

  _statusLine(line) {
    const version = /^HTTP\/(\d)\.(\d)$/.exec(line.split(' ')[0]);
    if (version === null) return null;
    const rest = line.slice(line.indexOf(' ') + 1);
    const code = Number.parseInt(rest, 10);
    if (Number.isNaN(code)) return null;
    return {
      statusCode: code,
      statusMessage: rest.slice(String(code).length).trim(),
      httpVersionMajor: Number(version[1]),
      httpVersionMinor: Number(version[2]),
      httpVersion: `${version[1]}.${version[2]}`,
    };
  }

  // A body that runs to the end of the connection is finished by the
  // connection ending, not by a count.
  finish() {
    if (this.state === 'length' && this.remaining === Infinity) this._complete();
  }

  _complete() {
    this.state = 'head';
    this.remaining = 0;
    this.handlers.complete();
  }
}

function collectHeaders(pairs) {
  const headers = {};
  for (const [rawName, value] of pairs) {
    const name = rawName.toLowerCase();
    const seen = headers[name];
    if (seen === undefined) {
      headers[name] = name === 'set-cookie' ? [value] : value;
    } else if (name === 'set-cookie') {
      seen.push(value);
    } else if (!SINGLE_VALUE.has(name)) {
      headers[name] = `${seen}, ${value}`;
    }
  }
  return headers;
}

// ------------------------------------------------------- incoming message ---

function IncomingMessage(socket) {
  if (!(this instanceof IncomingMessage)) return new IncomingMessage(socket);
  Readable.call(this, { autoDestroy: false });
  this.socket = socket;
  this.connection = socket;
  this.headers = {};
  this.rawHeaders = [];
  this.trailers = {};
  this.rawTrailers = [];
  this.httpVersion = '1.1';
  this.httpVersionMajor = 1;
  this.httpVersionMinor = 1;
  this.method = null;
  this.url = '';
  this.statusCode = null;
  this.statusMessage = null;
  this.complete = false;
}
Object.setPrototypeOf(IncomingMessage.prototype, Readable.prototype);
Object.setPrototypeOf(IncomingMessage, Readable);

IncomingMessage.prototype._read = function _read() {};

IncomingMessage.prototype.setTimeout = function setTimeout(timeout, callback) {
  if (this.socket) this.socket.setTimeout(timeout, callback);
  return this;
};

IncomingMessage.prototype._adopt = function _adopt(message) {
  Object.assign(this, message);
};

// --------------------------------------------------------- outgoing message ---

// Everything both a response and a request need: a header block that can still
// be changed until it is sent, and a body that is framed once it is.
function OutgoingMessage(socket) {
  if (!(this instanceof OutgoingMessage)) return new OutgoingMessage(socket);
  // The message finishes when its body is written; the socket underneath has
  // its own lifetime (keep-alive, a request finished before its response
  // exists), so the stream machinery must not destroy on finish.
  Writable.call(this, { autoDestroy: false });
  this.socket = socket;
  this.connection = socket;
  this.headersSent = false;
  this.finished = false;
  this._batch = null;
  this._ending = false;
  this._tail = null;
  this._last = false;
  this.sendDate = true;
  this.chunkedEncoding = false;
  this._headers = new Map();
  this._trailers = [];
}
Object.setPrototypeOf(OutgoingMessage.prototype, Writable.prototype);
Object.setPrototypeOf(OutgoingMessage, Writable);

OutgoingMessage.prototype.setHeader = function setHeader(name, value) {
  if (this.headersSent) {
    throw codedError('Cannot set headers after they are sent to the client',
      'ERR_HTTP_HEADERS_SENT');
  }
  if (typeof name !== 'string' || name === '') {
    throw codedError(`Header name must be a valid HTTP token ["${name}"]`,
      'ERR_INVALID_HTTP_TOKEN');
  }
  this._headers.set(name.toLowerCase(), [name, value]);
  return this;
};

OutgoingMessage.prototype.getHeader = function getHeader(name) {
  const found = this._headers.get(String(name).toLowerCase());
  return found === undefined ? undefined : found[1];
};

OutgoingMessage.prototype.getHeaders = function getHeaders() {
  const headers = {};
  for (const [key, [, value]] of this._headers) headers[key] = value;
  return headers;
};

OutgoingMessage.prototype.getHeaderNames = function getHeaderNames() {
  return [...this._headers.keys()];
};

OutgoingMessage.prototype.hasHeader = function hasHeader(name) {
  return this._headers.has(String(name).toLowerCase());
};

OutgoingMessage.prototype.removeHeader = function removeHeader(name) {
  if (this.headersSent) {
    throw codedError('Cannot remove headers after they are sent to the client',
      'ERR_HTTP_HEADERS_SENT');
  }
  this._headers.delete(String(name).toLowerCase());
};

OutgoingMessage.prototype.addTrailers = function addTrailers(headers) {
  const entries = headers instanceof Map ? [...headers] : Object.entries(headers ?? {});
  for (const [name, value] of entries) this._trailers.push([name, value]);
};

OutgoingMessage.prototype.setTimeout = function setTimeout(timeout, callback) {
  if (this.socket) this.socket.setTimeout(timeout, callback);
  return this;
};

OutgoingMessage.prototype._headerBlock = function _headerBlock(firstLine) {
  const lines = [firstLine];
  for (const [, [name, value]] of this._headers) {
    if (Array.isArray(value)) {
      for (const entry of value) lines.push(`${name}: ${entry}`);
    } else {
      lines.push(`${name}: ${value}`);
    }
  }
  return `${lines.join('\r\n')}\r\n\r\n`;
};

// The body is framed by whatever the header block promised: a length if one
// was given, chunks when the peer understands them, and the end of the
// connection otherwise (an HTTP/1.0 requester never sees chunks).
OutgoingMessage.prototype.useChunkedEncodingByDefault = true;

OutgoingMessage.prototype._decideFraming = function _decideFraming() {
  if (this._headers.has('content-length')) {
    this.chunkedEncoding = false;
    return;
  }
  // An explicit user-set Transfer-Encoding: chunked frames the body even for
  // a peer that would not get chunks by default.
  const te = this._headers.get('transfer-encoding');
  if (te !== undefined) {
    const value = Array.isArray(te[1]) ? te[1].join(',') : te[1];
    this.chunkedEncoding = /(?:^|\W)chunked(?:$|\W)/i.test(String(value));
    return;
  }
  if (this.useChunkedEncodingByDefault === false) {
    this.chunkedEncoding = false;
    return;
  }
  this.chunkedEncoding = true;
  this._headers.set('transfer-encoding', ['Transfer-Encoding', 'chunked']);
};

// Flush the header block and write `data` to the wire as-is — no body
// framing. `write` frames; this is the raw layer beneath it, and the corpus
// calls it directly to force per-write packets.
OutgoingMessage.prototype._send = function _send(data, encoding, callback) {
  if (typeof encoding === 'function') { callback = encoding; encoding = null; }
  if (!this.socket || this.socket.destroyed) {
    if (typeof callback === 'function') callback();
    return false;
  }
  if (!this.headersSent) this._sendHeaders();
  if (data != null && data.length !== 0) {
    this.socket.write(Buffer.isBuffer(data) ? data : Buffer.from(String(data), encoding || 'utf8'));
  }
  if (typeof callback === 'function') callback();
  return true;
};

// Everything one message write produces — header block, chunk framing, body —
// leaves in a single socket.write. Node flushes head plus first chunk with one
// writev, and corpus clients count the packets; separate writes race the
// client's 'data' events apart.
OutgoingMessage.prototype._raw = function _raw(data) {
  const buf = Buffer.isBuffer(data) ? data : Buffer.from(String(data), 'utf8');
  if (this._batch) this._batch.push(buf);
  else this.socket.write(buf);
};

OutgoingMessage.prototype._write = function _write(chunk, encoding, callback) {
  // A peer that tore the connection down mid-response makes every further
  // write meaningless, not an uncaught error; Node's socket swallows them
  // through its own error listener.
  if (!this.socket || this.socket.destroyed) { callback(); return; }
  const body = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), encoding || 'utf8');
  const parts = this._batch = [];
  if (!this.headersSent) this._sendHeaders();
  if (this.chunkedEncoding) {
    parts.push(Buffer.from(`${body.length.toString(16)}\r\n`), body, Buffer.from('\r\n'));
  } else {
    parts.push(body);
  }
  this._batch = null;
  if (this._ending) {
    // The chunk passed to end() shares its packet with whatever _final adds
    // (the chunked terminator) — Node flushes them with one writev.
    (this._tail ??= []).push(...parts);
  } else {
    this.socket.write(parts.length === 1 ? parts[0] : Buffer.concat(parts));
  }
  callback();
};

OutgoingMessage.prototype._final = function _final(callback) {
  if (this.socket && this.socket.destroyed) {
    this.finished = true;
    callback();
    return;
  }
  // A message that ends without a body announces an exact zero length
  // instead of an empty chunked stream, except where a body is forbidden
  // outright (1xx/204/304 responses) or the peer cannot take chunks anyway —
  // there the body is delimited by the connection closing, unannounced.
  if (!this.headersSent &&
      this.useChunkedEncodingByDefault !== false &&
      !this._headers.has('content-length') &&
      !this._headers.has('transfer-encoding')) {
    const code = this.statusCode;
    const bodyForbidden = typeof code === 'number' &&
      (code === 204 || code === 304 || (code >= 100 && code < 200));
    if (!bodyForbidden) {
      this._headers.set('content-length', ['Content-Length', '0']);
    }
  }
  const parts = this._batch = this._tail ?? [];
  this._tail = null;
  if (!this.headersSent) {
    // The header block leads the packet even when end()'s chunk was staged
    // first.
    const staged = parts.splice(0);
    this._sendHeaders();
    parts.push(...staged);
  }
  if (this.chunkedEncoding) {
    const trailers = this._trailers
      .map(([name, value]) => `${name}: ${value}\r\n`)
      .join('');
    parts.push(Buffer.from(`0\r\n${trailers}\r\n`));
  }
  this._batch = null;
  if (parts.length > 0) {
    this.socket.write(parts.length === 1 ? parts[0] : Buffer.concat(parts));
  }
  this.finished = true;
  this._finished();
  callback();
};

OutgoingMessage.prototype._finished = function _finished() {};

// `end()` marks the message finished synchronously, and `writable` stays
// `true` after a finished send — both are load-bearing Node quirks
// (nodejs/node#15029) the corpus asserts.
OutgoingMessage.prototype.end = function end(chunk, encoding, callback) {
  this._ending = true;
  Writable.prototype.end.call(this, chunk, encoding, callback);
  this.finished = true;
  return this;
};

Object.defineProperty(OutgoingMessage.prototype, 'writable', {
  get() { return !this.destroyed; },
  set(_v) {},
  configurable: true,
});

// `internal/http` exposes the raw header table under this symbol; tests read
// entries as `[Name, value]` pairs keyed by the lowercased name.
const kOutHeaders = Symbol('kOutHeaders');
Object.defineProperty(OutgoingMessage.prototype, kOutHeaders, {
  get() {
    const out = { __proto__: null };
    for (const [key, entry] of this._headers) out[key] = entry;
    return out;
  },
  configurable: true,
});

function ServerResponse(socket, request) {
  if (!(this instanceof ServerResponse)) return new ServerResponse(socket, request);
  OutgoingMessage.call(this, socket);
  this.statusCode = 200;
  this.statusMessage = undefined;
  this.shouldKeepAlive = true;
  this._request = request;
}
Object.setPrototypeOf(ServerResponse.prototype, OutgoingMessage.prototype);
Object.setPrototypeOf(ServerResponse, OutgoingMessage);

ServerResponse.prototype.writeHead = function writeHead(statusCode, statusMessage, headers) {
  if (typeof statusMessage === 'object' && statusMessage !== null) {
    headers = statusMessage;
    statusMessage = undefined;
  }
  if (this.headersSent) {
    throw codedError('Cannot render headers after they are sent to the client',
      'ERR_HTTP_HEADERS_SENT');
  }
  this.statusCode = statusCode;
  if (statusMessage !== undefined) this.statusMessage = statusMessage;
  if (headers) {
    if (Array.isArray(headers)) {
      for (let i = 0; i < headers.length; i += 2) this.setHeader(headers[i], headers[i + 1]);
    } else {
      for (const [name, value] of Object.entries(headers)) this.setHeader(name, value);
    }
  }
  return this;
};

ServerResponse.prototype.writeContinue = function writeContinue(callback) {
  this.socket.write('HTTP/1.1 100 Continue\r\n\r\n');
  if (typeof callback === 'function') callback();
};

ServerResponse.prototype._sendHeaders = function _sendHeaders() {
  this.headersSent = true;
  const message = this.statusMessage ?? STATUS_CODES[this.statusCode] ?? 'unknown';
  if (this.sendDate && !this._headers.has('date')) {
    this._headers.set('date', ['Date', new Date().toUTCString()]);
  }
  // Node's keep-alive contract, decided when the header block flushes:
  // an explicit user Connection header wins outright; otherwise the
  // connection persists only when the peer gave us a way to frame the body
  // (a user Content-Length, or chunked capability). `_last` — destroy after
  // this response — is separate from the header: a connection-delimited
  // body closes the socket even under a user-supplied keep-alive header.
  const code = this.statusCode;
  const hasBody = !(code === 204 || code === 304 || (code >= 100 && code < 200) ||
    this._request?.method === 'HEAD');
  // A bodiless status given a user Transfer-Encoding cannot chunk; Node
  // drops the framing and closes the connection instead (RFC 9110 §6.4.1).
  if (!hasBody && this._headers.has('transfer-encoding')) {
    this.shouldKeepAlive = false;
  }
  const conn = this._headers.get('connection');
  if (conn !== undefined) {
    const value = String(Array.isArray(conn[1]) ? conn[1].join(',') : conn[1]);
    if (/(?:^|\W)close(?:$|\W)/i.test(value)) this._last = true;
    else if (/(?:^|\W)keep-alive(?:$|\W)/i.test(value)) this.shouldKeepAlive = true;
  } else {
    const sendKeepAlive = this.shouldKeepAlive &&
      (this._headers.has('content-length') || this.useChunkedEncodingByDefault);
    if (!sendKeepAlive) this._last = true;
    this._headers.set('connection', ['Connection',
      sendKeepAlive ? 'keep-alive' : 'close']);
  }
  if (hasBody) {
    const hadTransferEncoding = this._headers.has('transfer-encoding');
    this._decideFraming();
    if (!this.chunkedEncoding && !this._headers.has('content-length')) {
      // No framing at all: the body runs to the end of the connection.
      this._last = true;
    }
    if (this.chunkedEncoding && !hadTransferEncoding) {
      // Re-insert so the block reads Connection before Transfer-Encoding.
      this._headers.delete('transfer-encoding');
      this._headers.set('transfer-encoding', ['Transfer-Encoding', 'chunked']);
    }
  } else {
    this.chunkedEncoding = false;
  }
  this._raw(this._headerBlock(`HTTP/1.1 ${this.statusCode} ${message}`));
};

// 'finish' itself comes from the stream machinery on the next tick — a
// synchronous emit here would let close() reap the connection as idle while
// the request handler is still running.
ServerResponse.prototype._finished = function _finished() {
  if (this._last) this.socket.end();
};

// ------------------------------------------------------------------ server ---

function Server(options, listener) {
  if (!(this instanceof Server)) return new Server(options, listener);
  EventEmitter.call(this);
  if (typeof options === 'function') { listener = options; options = {}; }
  this._options = options ?? {};
  this.timeout = 0;
  this.keepAliveTimeout = (options && options.keepAliveTimeout) ?? 5000;
  this._socket = net.createServer((socket) => this._connection(socket));
  this._socket.on('error', (error) => this.emit('error', error));
  this._socket.on('close', () => this.emit('close'));
  this._socket.on('listening', () => this.emit('listening'));
  if (typeof listener === 'function') this.on('request', listener);
}
Object.setPrototypeOf(Server.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Server, EventEmitter);

Object.defineProperty(Server.prototype, 'listening', {
  get() { return this._socket.listening; },
  configurable: true,
});

Server.prototype.listen = function listen(...args) {
  this._socket.listen(...args);
  return this;
};

Server.prototype.address = function address() {
  return this._socket.address();
};

Server.prototype.close = function close(callback) {
  // Since Node 19 `close` also ends idle keep-alive connections; a socket
  // with a request in flight ends when that response finishes instead of
  // waiting out its keep-alive window.
  this._closing = true;
  this.closeIdleConnections();
  this._socket.close(callback);
  return this;
};

Server.prototype.getConnections = function getConnections(callback) {
  return this._socket.getConnections(callback);
};

Server.prototype.setTimeout = function setTimeout(timeout, callback) {
  this.timeout = timeout;
  if (typeof callback === 'function') this.on('timeout', callback);
  return this;
};

Server.prototype.ref = function ref() { this._socket.ref(); return this; };
Server.prototype.unref = function unref() { this._socket.unref(); return this; };

Server.prototype.closeIdleConnections = function closeIdleConnections() {
  for (const socket of this._connectionSockets ?? []) {
    if ((socket._httpActiveRequests ?? 0) === 0) socket.destroy();
  }
};

Server.prototype.closeAllConnections = function closeAllConnections() {
  for (const socket of this._connectionSockets ?? []) socket.destroy();
};

Server.prototype._connection = function _connection(socket) {
  (this._connectionSockets ??= new Set()).add(socket);
  socket.once('close', () => this._connectionSockets.delete(socket));
  socket._httpActiveRequests = 0;
  this.emit('connection', socket);
  if (this.timeout > 0) socket.setTimeout(this.timeout);
  let request = null;
  const parser = new Parser('request', {
    upgrade: (message, head) => {
      const upgraded = new IncomingMessage(socket);
      upgraded._adopt(message);
      if (this.listenerCount('upgrade') > 0) {
        this.emit('upgrade', upgraded, socket, head);
      } else {
        // Nobody claims the socket, so nothing ever will speak on it again.
        socket.destroy();
      }
    },
    head: (message) => {
      request = new IncomingMessage(socket);
      request._adopt(message);
      const response = new ServerResponse(socket, request);
      socket._httpActiveRequests += 1;
      socket.setTimeout(0);
      response.once('finish', () => {
        socket._httpActiveRequests -= 1;
        if (socket._httpActiveRequests > 0 || socket.destroyed) return;
        // An idle kept-alive connection lives keepAliveTimeout, then goes;
        // a `_last` response already ended the socket itself.
        if (!response._last && this.keepAliveTimeout > 0) {
          socket.setTimeout(this.keepAliveTimeout, () => socket.destroy());
        }
      });
      response.shouldKeepAlive = keepAlive(message);
      // An HTTP/1.0 requester does not understand chunks — unless it said
      // `TE: chunked` — so its response body otherwise runs to the end of
      // the connection instead.
      response.useChunkedEncodingByDefault =
        (message.httpVersionMajor === 1 && message.httpVersionMinor >= 1) ||
        /(?:^|\W)chunked(?:$|\W)/i.test(String(message.headers.te ?? ''));
      const expects = String(message.headers.expect ?? '').toLowerCase();
      if (expects === '100-continue') {
        if (this.listenerCount('checkContinue') > 0) {
          this.emit('checkContinue', request, response);
          return;
        }
        response.writeContinue();
      }
      this.emit('request', request, response);
    },
    body: (chunk) => { if (request) request.push(chunk); },
    complete: () => {
      if (request) {
        request.complete = true;
        request.push(null);
        request = null;
      }
    },
    error: (error) => {
      this.emit('clientError', error, socket);
      socket.destroy();
    },
  });
  socket.on('data', (chunk) => parser.execute(chunk));
  socket.on('end', () => parser.finish());
  socket.on('error', () => {});
};

function keepAlive(message) {
  const connection = message.headers.connection;
  if (typeof connection === 'string') {
    const value = connection.toLowerCase();
    if (value.includes('close')) return false;
    if (value.includes('keep-alive')) return true;
  }
  return message.httpVersionMajor === 1 && message.httpVersionMinor >= 1;
}

// ------------------------------------------------------------------ client ---

function ClientRequest(options, callback) {
  if (!(this instanceof ClientRequest)) return new ClientRequest(options, callback);
  OutgoingMessage.call(this, null);
  const settings = normalizeClientOptions(options);
  this.method = settings.method;
  this.path = settings.path;
  this._settings = settings;
  this.aborted = false;
  this.reusedSocket = false;
  if (typeof callback === 'function') this.once('response', callback);

  const headers = settings.headers;
  if (Array.isArray(headers)) {
    // Either a flat [name, value, name, value] list or a list of pairs.
    if (Array.isArray(headers[0])) {
      for (const [name, value] of headers) this.setHeader(name, value);
    } else {
      for (let i = 0; i + 1 < headers.length; i += 2) this.setHeader(headers[i], headers[i + 1]);
    }
  } else {
    for (const [name, value] of Object.entries(headers)) this.setHeader(name, value);
  }
  if (!this._headers.has('host')) {
    const port = settings.port === settings.defaultPort ? '' : `:${settings.port}`;
    this._headers.set('host', ['Host', `${settings.host}${port}`]);
  }

  // `agent: false` means one throwaway connection; otherwise the named or
  // global agent owns the socket and may hand over a kept-alive one.
  const agent = settings.agent === false
    ? null
    : (settings.agent ?? module.exports.globalAgent);
  this.agent = agent;
  // Node marks keep-alive whenever an agent manages the socket at all —
  // pooled reuse rides the header even for agents without the keepAlive
  // option (their queue still reuses the live socket under maxSockets).
  this.shouldKeepAlive = agent !== null;
  if (agent === null) {
    this.onSocket(net.connect(settings.port, settings.host));
  } else {
    agent.addRequest(this, settings);
  }
}
Object.setPrototypeOf(ClientRequest.prototype, OutgoingMessage.prototype);
Object.setPrototypeOf(ClientRequest, OutgoingMessage);

// The agent (or the constructor, for agent-less requests) delivers the
// socket here — possibly a connected, kept-alive one.
ClientRequest.prototype.onSocket = function onSocket(socket) {
  this.socket = socket;
  this.connection = socket;
  const ready = () => {
    if (this.aborted) return;
    this.emit('socket', socket);
    this._flush();
  };
  if (socket.connecting) {
    socket.on('connect', ready);
  } else {
    queueMicrotask(ready);
  }
  socket.on('error', (error) => {
    // An abort tears the socket down on purpose; the wreckage is not an
    // error of the request.
    if (!this.aborted) this.emit('error', error);
  });
  if (this._settings.timeout != null) this.setTimeout(this._settings.timeout);
  this._listen();
};

ClientRequest.prototype._listen = function _listen() {
  let response = null;
  const parser = new Parser('response', {
    bodyless: this.method === 'HEAD',
    info: (message) => {
      // 100 Continue and friends: informational, the real response follows.
      if (message.statusCode === 100) this.emit('continue');
      else this.emit('information', message);
    },
    upgrade: (message, head) => {
      const res = new IncomingMessage(this.socket);
      res._adopt(message);
      if (this.listenerCount('upgrade') > 0) {
        this.emit('upgrade', res, this.socket, head);
      } else {
        this.socket.destroy();
      }
    },
    head: (message) => {
      // The server must agree to keep-alive; a response that says close (or
      // an HTTP/1.0 one that stays silent) makes the socket single-use.
      if (this.shouldKeepAlive && !keepAlive(message)) this.shouldKeepAlive = false;
      response = new IncomingMessage(this.socket);
      response._adopt(message);
      this.res = response;
      this.emit('response', response);
    },
    body: (chunk) => { if (response) response.push(chunk); },
    complete: () => {
      const socket = this.socket;
      socket.removeListener('data', this._parserOnData);
      socket.removeListener('end', this._parserOnEnd);
      const release = () => {
        if (this.agent && this.shouldKeepAlive && !socket.destroyed) {
          this.agent.freeSocket(socket, this._settings);
        } else if (this.agent) {
          socket.end();
          this.agent.removeSocket(socket, this._settings);
        } else {
          socket.end();
        }
      };
      const releaseWhenDone = () => {
        // Both directions must be done: the consumer drained the response
        // AND this request finished writing — a server may answer before
        // the request body went out, and the socket is not reusable until
        // it does.
        if (!this.finished) {
          this.once('finish', releaseWhenDone);
          return;
        }
        release();
      };
      if (response) {
        response.once('end', releaseWhenDone);
        response.complete = true;
        response.push(null);
        response = null;
      } else {
        releaseWhenDone();
      }
    },
    error: (error) => this.emit('error', error),
  });
  this._parserOnData = (chunk) => parser.execute(chunk);
  this._parserOnEnd = () => parser.finish();
  this.socket.on('data', this._parserOnData);
  this.socket.on('end', this._parserOnEnd);
};

// Nothing can go out before the connection exists, so the body written
// before then is held and sent in order once it does.
ClientRequest.prototype._write = function _write(chunk, encoding, callback) {
  if (!this.socket || this.socket.connecting) {
    (this._queued ??= []).push([chunk, encoding]);
    callback();
    return;
  }
  OutgoingMessage.prototype._write.call(this, chunk, encoding, callback);
};

ClientRequest.prototype._final = function _final(callback) {
  if (!this.socket || this.socket.connecting) {
    this._endPending = () => OutgoingMessage.prototype._final.call(this, () => {});
    callback();
    return;
  }
  OutgoingMessage.prototype._final.call(this, callback);
};

ClientRequest.prototype._flush = function _flush() {
  if (this.aborted) return;
  for (const [chunk, encoding] of this._queued ?? []) {
    OutgoingMessage.prototype._write.call(this, chunk, encoding, () => {});
  }
  this._queued = undefined;
  if (this._endPending) {
    const end = this._endPending;
    this._endPending = undefined;
    end();
  }
};

ClientRequest.prototype._sendHeaders = function _sendHeaders() {
  this.headersSent = true;
  if (this.method === 'GET' || this.method === 'HEAD' || this.method === 'DELETE') {
    // A request with no body says so by its absence, not by a framing header.
    if (!this._headers.has('content-length')) this.chunkedEncoding = false;
  } else {
    this._decideFraming();
  }
  if (!this._headers.has('connection')) {
    this._headers.set('connection', ['Connection',
      this.shouldKeepAlive ? 'keep-alive' : 'close']);
  } else if (/\bclose\b/i.test(String(this._headers.get('connection')[1]))) {
    // An explicit Connection: close promises the peer this socket dies with
    // the exchange — it must not go back to the pool.
    this.shouldKeepAlive = false;
  }
  this._raw(this._headerBlock(`${this.method} ${this.path} HTTP/1.1`));
};

ClientRequest.prototype.abort = function abort() {
  if (this.aborted) return;
  this.aborted = true;
  queueMicrotask(() => {
    this.emit('abort');
    this.emit('close');
  });
  this.destroy();
};

ClientRequest.prototype._destroy = function _destroy(error, callback) {
  this.aborted = true;
  if (this.socket) {
    this.socket.destroy();
    if (this.agent) this.agent.removeSocket(this.socket, this._settings);
  }
  callback(error);
};

ClientRequest.prototype.setTimeout = function setTimeout(timeout, callback) {
  if (this.socket) this.socket.setTimeout(timeout, callback);
  return this;
};

function decodedHostname(hostname) {
  try {
    return decodeURIComponent(hostname);
  } catch {
    return hostname;
  }
}

function normalizeClientOptions(options) {
  let settings = options;
  if (typeof options === 'string' || options instanceof URL) {
    const url = typeof options === 'string' ? new URL(options) : options;
    settings = {
      protocol: url.protocol,
      host: decodedHostname(url.hostname),
      port: url.port === '' ? undefined : Number(url.port),
      path: `${url.pathname}${url.search}`,
    };
  }
  const protocol = settings.protocol ?? 'http:';
  const defaultPort = Number(settings.defaultPort ?? (protocol === 'https:' ? 443 : 80));
  return {
    protocol,
    defaultPort,
    method: (settings.method ?? 'GET').toUpperCase(),
    host: settings.hostname ?? settings.host ?? 'localhost',
    port: Number(settings.port ?? defaultPort),
    path: settings.path ?? '/',
    headers: settings.headers ?? {},
    timeout: settings.timeout,
    agent: settings.agent,
    localAddress: settings.localAddress,
    family: settings.family,
    socketPath: settings.socketPath,
  };
}

// RFC 9110 token / field-value checks, exported through `_http_common`.
const TOKEN_RE = /^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$/;
function _checkIsHttpToken(value) {
  return typeof value === 'string' && TOKEN_RE.test(value);
}
const INVALID_FIELD_CHAR_RE = /[^\t\x20-\x7e\x80-\xff]/;
function _checkInvalidHeaderChar(value) {
  return typeof value === 'string' && INVALID_FIELD_CHAR_RE.test(value);
}

function createServer(options, listener) {
  return new Server(options, listener);
}

function request(options, second, third) {
  let callback = second;
  if (typeof second === 'object' && second !== null) {
    options = { ...normalizeClientOptions(options), ...second };
    callback = third;
  }
  return new ClientRequest(options, callback);
}

function get(options, second, third) {
  const client = request(options, second, third);
  client.end();
  return client;
}

// Keep-alive connection pool: sockets/freeSockets/requests keyed by
// getName, maxSockets queueing, socket reuse with the 'free' event.
function Agent(options) {
  if (!(this instanceof Agent)) return new Agent(options);
  EventEmitter.call(this);
  options = options || {};
  this.options = options;
  this.maxSockets = options.maxSockets ?? Infinity;
  this.maxTotalSockets = options.maxTotalSockets ?? Infinity;
  this.maxFreeSockets = options.maxFreeSockets ?? 256;
  this.keepAlive = options.keepAlive === true;
  this.keepAliveMsecs = options.keepAliveMsecs ?? 1000;
  this.scheduling = options.scheduling ?? 'lifo';
  this.totalSocketCount = 0;
  this.sockets = {};
  this.freeSockets = {};
  this.requests = {};
}
Object.setPrototypeOf(Agent.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Agent, EventEmitter);

Agent.prototype.defaultPort = 80;
Agent.prototype.protocol = 'http:';

Agent.prototype.createConnection = function createConnection(options, _callback) {
  return net.connect(options.port, options.host);
};

Agent.prototype.addRequest = function addRequest(req, options) {
  const name = this.getName(options);
  const free = this.freeSockets[name];
  if (free && free.length > 0) {
    const socket = this.scheduling === 'fifo' ? free.shift() : free.pop();
    if (free.length === 0) delete this.freeSockets[name];
    socket._clearTimer?.();
    (this.sockets[name] ??= []).push(socket);
    req.reusedSocket = true;
    req.onSocket(socket);
    return;
  }
  const active = (this.sockets[name] ??= []);
  if (active.length < this.maxSockets && this.totalSocketCount < this.maxTotalSockets) {
    let delivered = false;
    const deliver = (socket) => {
      if (delivered) return;
      delivered = true;
      this.totalSocketCount += 1;
      (this.sockets[name] ??= []).push(socket);
      socket._httpAgentName = name;
      socket.once('close', () => this._dropSocket(name, socket));
      req.onSocket(socket);
    };
    // Node's createConnection contract: the socket may come back as the
    // return value or through the (err, socket) callback — user overrides
    // use either. The connection options carry the agent's keep-alive
    // choice and its initial delay.
    const connOptions = {
      ...options,
      ...this.options,
      keepAlive: this.keepAlive,
      keepAliveInitialDelay: this.keepAliveMsecs,
    };
    const socket = this.createConnection(connOptions,
      (error, created) => {
        if (error) {
          if (!delivered) { delivered = true; req.emit('error', error); }
          return;
        }
        deliver(created);
      });
    if (socket) deliver(socket);
    return;
  }
  (this.requests[name] ??= []).push(req);
};

Agent.prototype._dropSocket = function _dropSocket(name, socket) {
  for (const table of [this.sockets, this.freeSockets]) {
    const list = table[name];
    if (!list) continue;
    const index = list.indexOf(socket);
    if (index !== -1) {
      list.splice(index, 1);
      this.totalSocketCount -= 1;
      if (list.length === 0) delete table[name];
    }
  }
  this._serviceQueue(name);
};

Agent.prototype._serviceQueue = function _serviceQueue(name) {
  const queue = this.requests[name];
  if (!queue || queue.length === 0) return;
  const active = this.sockets[name] ?? [];
  if (active.length >= this.maxSockets || this.totalSocketCount >= this.maxTotalSockets) return;
  const req = queue.shift();
  if (queue.length === 0) delete this.requests[name];
  this.addRequest(req, req._settings);
};

// A request finished with its response; the socket comes back to the pool
// or dies, and a queued request takes the slot either way.
Agent.prototype.freeSocket = function freeSocket(socket, options) {
  const name = socket._httpAgentName ?? this.getName(options ?? {});
  const active = this.sockets[name];
  if (active) {
    const index = active.indexOf(socket);
    if (index !== -1) active.splice(index, 1);
    if (active.length === 0) delete this.sockets[name];
  }
  const queue = this.requests[name];
  if (queue && queue.length > 0 && !socket.destroyed) {
    const req = queue.shift();
    if (queue.length === 0) delete this.requests[name];
    (this.sockets[name] ??= []).push(socket);
    req.reusedSocket = true;
    req.onSocket(socket);
    return;
  }
  if (this.keepAlive && !socket.destroyed) {
    const free = (this.freeSockets[name] ??= []);
    if (free.length < this.maxFreeSockets) {
      socket.unref?.();
      free.push(socket);
      this.emit('free', socket, options ?? {});
      this._serviceQueue(name);
      return;
    }
  }
  socket.destroy();
  this._serviceQueue(name);
};

Agent.prototype.removeSocket = function removeSocket(socket, options) {
  this._dropSocket(socket._httpAgentName ?? this.getName(options ?? {}), socket);
};

Agent.prototype.destroy = function destroy() {
  for (const table of [this.sockets, this.freeSockets]) {
    for (const name of Object.keys(table)) {
      for (const socket of [...table[name]]) socket.destroy();
    }
  }
};

// §https://nodejs.org/api/http.html#agentgetnameoptions — the pool key:
// `host:port:localAddress`, with `:family` when one of 4/6 was named and
// `:socketPath` for a unix socket.
Agent.prototype.getName = function getName(options = {}) {
  let name = `${options.host || 'localhost'}:`;
  if (options.port) name += options.port;
  name += ':';
  if (options.localAddress) name += options.localAddress;
  if (options.family === 4 || options.family === 6) name += `:${options.family}`;
  if (options.socketPath) name += `:${options.socketPath}`;
  return name;
};

module.exports = {
  STATUS_CODES,
  METHODS,
  _checkIsHttpToken,
  _checkInvalidHeaderChar,
  _kOutHeaders: kOutHeaders,
  Agent,
  ClientRequest,
  IncomingMessage,
  OutgoingMessage,
  Server,
  ServerResponse,
  createServer,
  request,
  get,
  globalAgent: new Agent({ keepAlive: true, timeout: 5000 }),
  maxHeaderSize: 16384,
  validateHeaderName(name) {
    if (typeof name !== 'string' || !/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(name)) {
      throw codedError(`Header name must be a valid HTTP token ["${name}"]`,
        'ERR_INVALID_HTTP_TOKEN');
    }
  },
  validateHeaderValue(name, value) {
    if (value === undefined) {
      throw codedError(`Invalid value "${value}" for header "${name}"`,
        'ERR_HTTP_INVALID_HEADER_VALUE');
    }
  },
};
