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

class IncomingMessage extends Readable {
  constructor(socket) {
    super();
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

  _read() {}

  setTimeout(timeout, callback) {
    if (this.socket) this.socket.setTimeout(timeout, callback);
    return this;
  }

  _adopt(message) {
    Object.assign(this, message);
  }
}

// --------------------------------------------------------- outgoing message ---

// Everything both a response and a request need: a header block that can still
// be changed until it is sent, and a body that is framed once it is.
class OutgoingMessage extends Writable {
  constructor(socket) {
    super();
    this.socket = socket;
    this.connection = socket;
    this.headersSent = false;
    this.finished = false;
    this.sendDate = true;
    this.chunkedEncoding = false;
    this._headers = new Map();
    this._trailers = [];
  }

  setHeader(name, value) {
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
  }

  getHeader(name) {
    const found = this._headers.get(String(name).toLowerCase());
    return found === undefined ? undefined : found[1];
  }

  getHeaders() {
    const headers = {};
    for (const [key, [, value]] of this._headers) headers[key] = value;
    return headers;
  }

  getHeaderNames() {
    return [...this._headers.keys()];
  }

  hasHeader(name) {
    return this._headers.has(String(name).toLowerCase());
  }

  removeHeader(name) {
    if (this.headersSent) {
      throw codedError('Cannot remove headers after they are sent to the client',
        'ERR_HTTP_HEADERS_SENT');
    }
    this._headers.delete(String(name).toLowerCase());
  }

  addTrailers(headers) {
    const entries = headers instanceof Map ? [...headers] : Object.entries(headers ?? {});
    for (const [name, value] of entries) this._trailers.push([name, value]);
  }

  setTimeout(timeout, callback) {
    if (this.socket) this.socket.setTimeout(timeout, callback);
    return this;
  }

  _headerBlock(firstLine) {
    const lines = [firstLine];
    for (const [, [name, value]] of this._headers) {
      if (Array.isArray(value)) {
        for (const entry of value) lines.push(`${name}: ${entry}`);
      } else {
        lines.push(`${name}: ${value}`);
      }
    }
    return `${lines.join('\r\n')}\r\n\r\n`;
  }

  // The body is framed by whatever the header block promised: a length if one
  // was given, chunks otherwise.
  _decideFraming() {
    if (this._headers.has('content-length')) {
      this.chunkedEncoding = false;
      return;
    }
    this.chunkedEncoding = true;
    this._headers.set('transfer-encoding', ['Transfer-Encoding', 'chunked']);
  }

  _write(chunk, encoding, callback) {
    if (!this.headersSent) this._sendHeaders();
    const body = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), encoding || 'utf8');
    if (this.chunkedEncoding) {
      this.socket.write(`${body.length.toString(16)}\r\n`);
      this.socket.write(body);
      this.socket.write('\r\n');
    } else {
      this.socket.write(body);
    }
    callback();
  }

  _final(callback) {
    if (!this.headersSent) this._sendHeaders();
    if (this.chunkedEncoding) {
      const trailers = this._trailers
        .map(([name, value]) => `${name}: ${value}\r\n`)
        .join('');
      this.socket.write(`0\r\n${trailers}\r\n`);
    }
    this.finished = true;
    this._finished();
    callback();
  }

  _finished() {}
}

class ServerResponse extends OutgoingMessage {
  constructor(socket, request) {
    super(socket);
    this.statusCode = 200;
    this.statusMessage = undefined;
    this.shouldKeepAlive = true;
    this._request = request;
  }

  writeHead(statusCode, statusMessage, headers) {
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
  }

  writeContinue(callback) {
    this.socket.write('HTTP/1.1 100 Continue\r\n\r\n');
    if (typeof callback === 'function') callback();
  }

  _sendHeaders() {
    this.headersSent = true;
    const message = this.statusMessage ?? STATUS_CODES[this.statusCode] ?? 'unknown';
    if (this.sendDate && !this._headers.has('date')) {
      this._headers.set('date', ['Date', new Date().toUTCString()]);
    }
    this._decideFraming();
    if (!this._headers.has('connection')) {
      this._headers.set('connection', ['Connection',
        this.shouldKeepAlive ? 'keep-alive' : 'close']);
    }
    this.socket.write(this._headerBlock(`HTTP/1.1 ${this.statusCode} ${message}`));
  }

  _finished() {
    this.emit('finish');
    if (!this.shouldKeepAlive) this.socket.end();
  }
}

// ------------------------------------------------------------------ server ---

class Server extends EventEmitter {
  constructor(options, listener) {
    super();
    if (typeof options === 'function') { listener = options; options = {}; }
    this._options = options ?? {};
    this.timeout = 0;
    this._socket = net.createServer((socket) => this._connection(socket));
    this._socket.on('error', (error) => this.emit('error', error));
    this._socket.on('close', () => this.emit('close'));
    this._socket.on('listening', () => this.emit('listening'));
    if (typeof listener === 'function') this.on('request', listener);
  }

  get listening() {
    return this._socket.listening;
  }

  listen(...args) {
    this._socket.listen(...args);
    return this;
  }

  address() {
    return this._socket.address();
  }

  close(callback) {
    this._socket.close(callback);
    return this;
  }

  getConnections(callback) {
    return this._socket.getConnections(callback);
  }

  setTimeout(timeout, callback) {
    this.timeout = timeout;
    if (typeof callback === 'function') this.on('timeout', callback);
    return this;
  }

  ref() { this._socket.ref(); return this; }
  unref() { this._socket.unref(); return this; }

  _connection(socket) {
    this.emit('connection', socket);
    if (this.timeout > 0) socket.setTimeout(this.timeout);
    let request = null;
    const parser = new Parser('request', {
      head: (message) => {
        request = new IncomingMessage(socket);
        request._adopt(message);
        const response = new ServerResponse(socket, request);
        response.shouldKeepAlive = keepAlive(message);
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
  }
}

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

class ClientRequest extends OutgoingMessage {
  constructor(options, callback) {
    super(null);
    const settings = normalizeClientOptions(options);
    this.method = settings.method;
    this.path = settings.path;
    this._settings = settings;
    this.shouldKeepAlive = false;
    if (typeof callback === 'function') this.once('response', callback);

    for (const [name, value] of Object.entries(settings.headers)) this.setHeader(name, value);
    if (!this._headers.has('host')) {
      const port = settings.port === 80 ? '' : `:${settings.port}`;
      this._headers.set('host', ['Host', `${settings.host}${port}`]);
    }

    this.socket = net.connect(settings.port, settings.host);
    this.connection = this.socket;
    this.socket.on('connect', () => {
      this.emit('socket', this.socket);
      this._flush();
    });
    this.socket.on('error', (error) => this.emit('error', error));
    this._listen();
  }

  _listen() {
    let response = null;
    const parser = new Parser('response', {
      bodyless: this.method === 'HEAD',
      head: (message) => {
        response = new IncomingMessage(this.socket);
        response._adopt(message);
        this.res = response;
        this.emit('response', response);
      },
      body: (chunk) => { if (response) response.push(chunk); },
      complete: () => {
        if (response) {
          response.complete = true;
          response.push(null);
          response = null;
        }
        this.socket.end();
      },
      error: (error) => this.emit('error', error),
    });
    this.socket.on('data', (chunk) => parser.execute(chunk));
    this.socket.on('end', () => parser.finish());
  }

  // Nothing can go out before the connection exists, so the body written
  // before then is held and sent in order once it does.
  _write(chunk, encoding, callback) {
    if (this.socket.connecting) {
      (this._queued ??= []).push([chunk, encoding]);
      callback();
      return;
    }
    super._write(chunk, encoding, callback);
  }

  _final(callback) {
    if (this.socket.connecting) {
      this._endPending = () => super._final(() => {});
      callback();
      return;
    }
    super._final(callback);
  }

  _flush() {
    for (const [chunk, encoding] of this._queued ?? []) {
      super._write(chunk, encoding, () => {});
    }
    this._queued = undefined;
    if (this._endPending) {
      const end = this._endPending;
      this._endPending = undefined;
      end();
    }
  }

  _sendHeaders() {
    this.headersSent = true;
    if (this.method === 'GET' || this.method === 'HEAD' || this.method === 'DELETE') {
      // A request with no body says so by its absence, not by a framing header.
      if (!this._headers.has('content-length')) this.chunkedEncoding = false;
    } else {
      this._decideFraming();
    }
    if (!this._headers.has('connection')) {
      this._headers.set('connection', ['Connection', 'close']);
    }
    this.socket.write(this._headerBlock(`${this.method} ${this.path} HTTP/1.1`));
  }

  abort() {
    this.destroy();
  }

  _destroy(error, callback) {
    if (this.socket) this.socket.destroy();
    callback(error);
  }

  setTimeout(timeout, callback) {
    if (this.socket) this.socket.setTimeout(timeout, callback);
    return this;
  }
}

function normalizeClientOptions(options) {
  let settings = options;
  if (typeof options === 'string') {
    const url = new URL(options);
    settings = {
      host: url.hostname,
      port: url.port === '' ? 80 : Number(url.port),
      path: `${url.pathname}${url.search}`,
    };
  } else if (options instanceof URL) {
    settings = {
      host: options.hostname,
      port: options.port === '' ? 80 : Number(options.port),
      path: `${options.pathname}${options.search}`,
    };
  }
  return {
    method: (settings.method ?? 'GET').toUpperCase(),
    host: settings.hostname ?? settings.host ?? 'localhost',
    port: Number(settings.port ?? 80),
    path: settings.path ?? '/',
    headers: settings.headers ?? {},
  };
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

// A connection per request, which is what `Connection: close` above says.
class Agent extends EventEmitter {
  constructor(options = {}) {
    super();
    this.options = options;
    this.maxSockets = options.maxSockets ?? Infinity;
    this.maxFreeSockets = options.maxFreeSockets ?? 256;
    this.keepAlive = options.keepAlive === true;
    this.sockets = {};
    this.freeSockets = {};
    this.requests = {};
  }

  destroy() {}
  getName(options) {
    return `${options?.host ?? 'localhost'}:${options?.port ?? ''}`;
  }
}

module.exports = {
  STATUS_CODES,
  METHODS,
  Agent,
  ClientRequest,
  IncomingMessage,
  OutgoingMessage,
  Server,
  ServerResponse,
  createServer,
  request,
  get,
  globalAgent: new Agent(),
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
