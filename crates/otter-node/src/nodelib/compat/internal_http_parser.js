'use strict';
// internalBinding('http_parser') — llhttp's JS-visible contract (HTTPParser
// with indexed kOn* callback slots, methods tables, ConnectionsList) over an
// incremental JavaScript state machine. The vendored _http_* files are the
// only consumers; they drive it exactly the way Node drives the native
// binding: repeated execute(Buffer) calls, finish() at EOF, pause()/resume()
// for backpressure, and numeric returns that only matter at upgrade
// boundaries (bodyHead = d.slice(ret)).

const { Buffer } = require('buffer');

const methods = [
  'DELETE', 'GET', 'HEAD', 'POST', 'PUT', 'CONNECT', 'OPTIONS', 'TRACE',
  'COPY', 'LOCK', 'MKCOL', 'MOVE', 'PROPFIND', 'PROPPATCH', 'SEARCH',
  'UNLOCK', 'BIND', 'REBIND', 'UNBIND', 'ACL', 'REPORT', 'MKACTIVITY',
  'CHECKOUT', 'MERGE', 'M-SEARCH', 'NOTIFY', 'SUBSCRIBE', 'UNSUBSCRIBE',
  'PATCH', 'PURGE', 'MKCALENDAR', 'LINK', 'UNLINK', 'SOURCE',
];
const allMethods = [...methods, 'PRI', 'QUERY'];
const methodIndex = new Map(allMethods.map((m, i) => [m, i]));

const kOnMessageBegin = 0;
const kOnHeaders = 1;
const kOnHeadersComplete = 2;
const kOnBody = 3;
const kOnMessageComplete = 4;
const kOnExecute = 5;
const kOnTimeout = 6;

const DEFAULT_MAX_HEADER_SIZE = 16 * 1024;
const CRLF = Buffer.from('\r\n');

function parseError(code, reason) {
  const err = new Error(`Parse Error: ${reason}`);
  err.code = code;
  err.reason = reason;
  err.bytesParsed = 0;
  return err;
}

const TOKEN = /^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$/;

class ConnectionsList {
  constructor() {
    this._parsers = new Set();
  }
  all() {
    return [...this._parsers];
  }
  idle() {
    return [...this._parsers].filter((p) => p._messageStart === 0);
  }
  expired(headersTimeout, requestTimeout) {
    const now = Date.now();
    const out = [];
    for (const p of this._parsers) {
      if (p._messageStart === 0) continue;
      if (headersTimeout > 0 && !p._headersDone &&
          now - p._messageStart >= headersTimeout) {
        out.push(p);
      } else if (requestTimeout > 0 && now - p._messageStart >= requestTimeout) {
        out.push(p);
      }
    }
    return out;
  }
}

class HTTPParser {
  constructor() {
    this[kOnMessageBegin] = null;
    this[kOnHeaders] = null;
    this[kOnHeadersComplete] = null;
    this[kOnBody] = null;
    this[kOnMessageComplete] = null;
    this[kOnExecute] = null;
    this[kOnTimeout] = null;
    this._reset(0);
  }

  _reset(type) {
    this._type = type;
    this._stash = Buffer.alloc(0);
    this._paused = false;
    this._upgraded = false;
    this._maxHeaderSize = DEFAULT_MAX_HEADER_SIZE;
    this._connections = null;
    this._messageStart = 0;
    this._headersDone = false;
    this._resetMessage();
  }

  _resetMessage() {
    // 'line' → 'headers' → one of the body states → back to 'line'.
    this._state = 'line';
    this._rawHeaders = [];
    this._headerBytes = 0;
    this._versionMajor = 0;
    this._versionMinor = 0;
    this._method = -1;
    this._urlValue = '';
    this._statusCode = 0;
    this._statusMessage = '';
    this._upgradeFlag = false;
    this._keepAlive = false;
    this._contentLength = -1;
    this._chunked = false;
    this._remaining = 0;
    this._skipBody = false;
    this._trailerLines = [];
  }

  initialize(type, _resource, maxHeaderSize, _lenientFlags, connectionsList) {
    this._reset(type);
    if (typeof maxHeaderSize === 'number' && maxHeaderSize > 0) {
      this._maxHeaderSize = maxHeaderSize;
    }
    if (connectionsList instanceof ConnectionsList) {
      this._connections = connectionsList;
      connectionsList._parsers.add(this);
    }
  }

  close() {}
  free() {}
  consume(_handle) {}
  unconsume() {}
  remove() {
    if (this._connections) {
      this._connections._parsers.delete(this);
      this._connections = null;
    }
  }
  pause() {
    this._paused = true;
  }
  resume() {
    if (!this._paused) return;
    this._paused = false;
    if (this._stash.length > 0 && !this._upgraded) {
      const ret = this._run(0);
      const onExecute = this[kOnExecute];
      if (typeof onExecute === 'function') {
        onExecute.call(this, ret);
      }
    }
  }
  getCurrentBuffer() {
    return this._stash;
  }
  duration() {
    return 0;
  }
  headersCompleted() {
    return this._headersDone;
  }

  finish() {
    if (this._upgraded || this._paused) return;
    if (this._state === 'line' && this._stash.length === 0) return;
    if (this._state === 'body-eof') {
      this._finishMessage();
      return;
    }
    return parseError('HPE_INVALID_EOF_STATE', 'Invalid EOF state');
  }

  execute(d) {
    if (this._upgraded) {
      return parseError('HPE_PAUSED_UPGRADE', 'Pause on CONNECT/Upgrade');
    }
    const prior = this._stash.length;
    this._stash = prior === 0 ? d : Buffer.concat([this._stash, d]);
    return this._run(prior, d.length);
  }

  // Drive the state machine over the stash. `prior` is how many stash bytes
  // predate the current execute()'s chunk — upgrade returns are indexes into
  // that chunk. Without a current chunk (resume) returns count stash bytes.
  _run(prior, chunkLength = this._stash.length - prior) {
    let consumed = 0;
    for (;;) {
      if (this._paused || this._upgraded) break;
      const before = this._stash.length;
      const result = this._step();
      consumed += before - this._stash.length;
      if (result === 'wait') break;
      if (result instanceof Error) {
        result.bytesParsed = Math.max(0, Math.min(consumed - prior, chunkLength));
        return result;
      }
      if (result === 'upgraded') {
        return Math.max(0, Math.min(consumed - prior, chunkLength));
      }
    }
    return chunkLength;
  }

  // One transition. Returns 'more' (progress), 'wait' (need bytes),
  // 'upgraded', or an Error.
  _step() {
    const state = this._state;
    if (state === 'line') {
      if (this._stash.length === 0) return 'wait';
      // llhttp rejects an impossible method byte-by-byte, without waiting
      // for the line to complete.
      if (this._type === HTTPParser.REQUEST && this._stash[0] !== 13) {
        const err = this._checkMethodPrefix();
        if (err) return err;
      }
      const idx = this._stash.indexOf(CRLF);
      if (idx === -1) {
        if (this._stash.length > this._maxHeaderSize) {
          return parseError('HPE_HEADER_OVERFLOW', 'Header overflow');
        }
        if (this._stash.length > 0 && this._messageStart === 0) {
          this._messageStart = Date.now();
        }
        return 'wait';
      }
      // llhttp skips blank lines before a request line; they do not start
      // a message, so the header-timeout clock stays unarmed.
      if (idx === 0) {
        this._stash = this._stash.subarray(2);
        this._messageStart = 0;
        return 'more';
      }
      if (this._messageStart === 0) this._messageStart = Date.now();
      const line = this._stash.subarray(0, idx).toString('latin1');
      const err = this._type === HTTPParser.RESPONSE
        ? this._parseStatusLine(line)
        : this._parseRequestLine(line);
      if (err) return err;
      this._stash = this._stash.subarray(idx + 2);
      this._headerBytes = idx + 2;
      this._state = 'headers';
      const onBegin = this[kOnMessageBegin];
      if (typeof onBegin === 'function') onBegin.call(this);
      return 'more';
    }
    if (state === 'headers') {
      const idx = this._stash.indexOf(CRLF);
      if (idx === -1) {
        if (this._headerBytes + this._stash.length > this._maxHeaderSize) {
          return parseError('HPE_HEADER_OVERFLOW', 'Header overflow');
        }
        return 'wait';
      }
      this._headerBytes += idx + 2;
      if (this._headerBytes > this._maxHeaderSize) {
        return parseError('HPE_HEADER_OVERFLOW', 'Header overflow');
      }
      const line = this._stash.subarray(0, idx).toString('latin1');
      this._stash = this._stash.subarray(idx + 2);
      if (line.length === 0) {
        return this._headersComplete();
      }
      if (line[0] === ' ' || line[0] === '\t') {
        // obs-fold: continuation joins the previous value.
        const n = this._rawHeaders.length;
        if (n === 0) {
          return parseError('HPE_INVALID_HEADER_TOKEN', 'Invalid header token');
        }
        this._rawHeaders[n - 1] += ` ${line.trim()}`;
        return 'more';
      }
      const colon = line.indexOf(':');
      if (colon <= 0) {
        return parseError('HPE_INVALID_HEADER_TOKEN', 'Invalid header token');
      }
      const name = line.slice(0, colon);
      if (!TOKEN.test(name)) {
        return parseError('HPE_INVALID_HEADER_TOKEN', 'Invalid header token');
      }
      this._rawHeaders.push(name, line.slice(colon + 1).trim());
      return 'more';
    }
    if (state === 'body-length') {
      if (this._stash.length === 0) return 'wait';
      const take = Math.min(this._remaining, this._stash.length);
      const chunk = this._stash.subarray(0, take);
      this._stash = this._stash.subarray(take);
      this._remaining -= take;
      this._emitBody(chunk);
      if (this._remaining === 0) return this._messageComplete();
      return 'wait';
    }
    if (state === 'body-eof') {
      if (this._stash.length === 0) return 'wait';
      const chunk = this._stash;
      this._stash = Buffer.alloc(0);
      this._emitBody(chunk);
      return 'wait';
    }
    if (state === 'chunk-size') {
      const idx = this._stash.indexOf(CRLF);
      if (idx === -1) {
        if (this._stash.length > 1024) {
          return parseError('HPE_INVALID_CHUNK_SIZE', 'Invalid character in chunk size');
        }
        return 'wait';
      }
      const line = this._stash.subarray(0, idx).toString('latin1');
      const sizeToken = line.split(';', 1)[0].trim();
      if (!/^[0-9a-fA-F]+$/.test(sizeToken)) {
        return parseError('HPE_INVALID_CHUNK_SIZE', 'Invalid character in chunk size');
      }
      this._stash = this._stash.subarray(idx + 2);
      this._remaining = Number.parseInt(sizeToken, 16);
      this._state = this._remaining === 0 ? 'trailers' : 'chunk-data';
      return 'more';
    }
    if (state === 'chunk-data') {
      if (this._stash.length === 0) return 'wait';
      const take = Math.min(this._remaining, this._stash.length);
      const chunk = this._stash.subarray(0, take);
      this._stash = this._stash.subarray(take);
      this._remaining -= take;
      this._emitBody(chunk);
      if (this._remaining === 0) this._state = 'chunk-crlf';
      return this._remaining === 0 ? 'more' : 'wait';
    }
    if (state === 'chunk-crlf') {
      if (this._stash.length < 2) return 'wait';
      if (this._stash[0] !== 13 || this._stash[1] !== 10) {
        return parseError('HPE_INVALID_CHUNK_SIZE', 'Expected CRLF after chunk data');
      }
      this._stash = this._stash.subarray(2);
      this._state = 'chunk-size';
      return 'more';
    }
    if (state === 'trailers') {
      const idx = this._stash.indexOf(CRLF);
      if (idx === -1) return 'wait';
      const line = this._stash.subarray(0, idx).toString('latin1');
      this._stash = this._stash.subarray(idx + 2);
      if (line.length === 0) {
        if (this._trailerLines.length > 0) {
          const onHeaders = this[kOnHeaders];
          if (typeof onHeaders === 'function') {
            onHeaders.call(this, this._trailerLines, '');
          }
        }
        return this._messageComplete();
      }
      const colon = line.indexOf(':');
      if (colon > 0) {
        this._trailerLines.push(line.slice(0, colon), line.slice(colon + 1).trim());
      }
      return 'more';
    }
    return 'wait';
  }

  // The method token seen so far (up to the first space or end of buffered
  // data) must be a prefix of some known method.
  _checkMethodPrefix() {
    const stash = this._stash;
    const limit = Math.min(stash.length, 24);
    let end = limit;
    for (let i = 0; i < limit; i++) {
      if (stash[i] === 32) { end = i; break; }
    }
    const token = stash.subarray(0, end).toString('latin1');
    for (const method of allMethods) {
      if (method.startsWith(token)) return null;
    }
    return parseError('HPE_INVALID_METHOD', 'Invalid method encountered');
  }

  _parseRequestLine(line) {
    const parts = line.split(' ');
    if (parts.length < 3) {
      return parseError('HPE_INVALID_METHOD', 'Invalid method encountered');
    }
    const method = parts[0];
    const version = parts[parts.length - 1];
    const url = parts.slice(1, -1).join(' ');
    const index = methodIndex.get(method);
    if (index === undefined) {
      return parseError('HPE_INVALID_METHOD', 'Invalid method encountered');
    }
    const m = /^HTTP\/(\d)\.(\d)$/.exec(version);
    if (m === null) {
      return parseError('HPE_INVALID_VERSION', 'Invalid HTTP version');
    }
    this._method = index;
    this._urlValue = url;
    this._versionMajor = Number(m[1]);
    this._versionMinor = Number(m[2]);
    return null;
  }

  _parseStatusLine(line) {
    const m = /^HTTP\/(\d)\.(\d) (\d{3})(?: (.*))?$/.exec(line);
    if (m === null) {
      return parseError('HPE_INVALID_CONSTANT', 'Expected HTTP/');
    }
    this._versionMajor = Number(m[1]);
    this._versionMinor = Number(m[2]);
    this._statusCode = Number(m[3]);
    this._statusMessage = m[4] ?? '';
    return null;
  }

  _headersComplete() {
    const raw = this._rawHeaders;
    let connection = '';
    let upgradeHeader = false;
    let contentLength = -1;
    let chunked = false;
    let transferEncoding = false;
    for (let i = 0; i < raw.length; i += 2) {
      const name = raw[i].toLowerCase();
      const value = raw[i + 1];
      if (name === 'connection') {
        connection += `${connection.length > 0 ? ',' : ''}${value.toLowerCase()}`;
      } else if (name === 'upgrade') {
        upgradeHeader = true;
      } else if (name === 'content-length') {
        const parsed = /^\d+$/.test(value.trim()) ? Number(value.trim()) : NaN;
        if (!Number.isFinite(parsed)) {
          return parseError('HPE_INVALID_CONTENT_LENGTH', 'Invalid character in Content-Length');
        }
        if (contentLength !== -1 && contentLength !== parsed) {
          return parseError('HPE_INVALID_CONTENT_LENGTH', 'Duplicate Content-Length');
        }
        contentLength = parsed;
      } else if (name === 'transfer-encoding') {
        transferEncoding = true;
        if (/(?:^|\W)chunked(?:$|\W)/i.test(value)) chunked = true;
      }
    }
    const versionOnePlus = this._versionMajor === 1 && this._versionMinor >= 1;
    let keepAlive = versionOnePlus || this._versionMajor > 1;
    if (/(?:^|\W)close(?:$|\W)/.test(connection)) keepAlive = false;
    else if (/(?:^|\W)keep-alive(?:$|\W)/.test(connection)) keepAlive = true;
    const isConnect = this._type === HTTPParser.REQUEST &&
      allMethods[this._method] === 'CONNECT';
    const upgrade = isConnect ||
      (upgradeHeader && /(?:^|\W)upgrade(?:$|\W)/.test(connection)) ||
      this._statusCode === 101;
    this._headersDone = true;
    this._upgradeFlag = upgrade;
    this._chunked = chunked;
    this._contentLength = contentLength;

    const isResponse = this._type === HTTPParser.RESPONSE;
    if (isResponse && !chunked && contentLength === -1 &&
        this._statusCode !== 204 && this._statusCode !== 304 &&
        !(this._statusCode >= 100 && this._statusCode < 200)) {
      keepAlive = false;
    }
    this._keepAlive = keepAlive;

    const cb = this[kOnHeadersComplete];
    let ret = 0;
    if (typeof cb === 'function') {
      ret = cb.call(
        this,
        this._versionMajor,
        this._versionMinor,
        raw,
        isResponse ? undefined : this._method,
        this._urlValue,
        isResponse ? this._statusCode : undefined,
        isResponse ? this._statusMessage : undefined,
        upgrade,
        keepAlive,
      ) | 0;
    }
    // An upgrade request still carries its declared body (llhttp parses it
    // and only then reports the upgrade index); CONNECT never has one.
    this._skipBody = ret === 1 || ret === 2 || isConnect;
    // §llhttp — a Transfer-Encoding whose final coding is not chunked has
    // no defined body framing for a request; the message errors after the
    // headers callback, so the request object exists but sees no body.
    if (transferEncoding && !chunked && !this._skipBody &&
        this._type === HTTPParser.REQUEST && contentLength === -1) {
      return parseError('HPE_INVALID_TRANSFER_ENCODING', 'Invalid transfer encoding');
    }

    const bodyless = isResponse &&
      (this._statusCode === 204 || this._statusCode === 304 ||
       (this._statusCode >= 100 && this._statusCode < 200));
    if (this._skipBody || bodyless) {
      return this._messageComplete();
    }
    if (chunked) {
      this._state = 'chunk-size';
      return 'more';
    }
    if (contentLength !== -1) {
      if (contentLength === 0) return this._messageComplete();
      this._remaining = contentLength;
      this._state = 'body-length';
      return 'more';
    }
    if (isResponse) {
      // Delimited by the connection closing; finish() completes it.
      this._state = 'body-eof';
      return 'more';
    }
    return this._messageComplete();
  }

  _emitBody(chunk) {
    const cb = this[kOnBody];
    if (typeof cb === 'function' && chunk.length > 0) cb.call(this, chunk);
  }

  _finishMessage() {
    this._state = 'line';
    this._messageStart = 0;
    this._headersDone = false;
    const upgraded = this._upgradeFlag;
    const cb = this[kOnMessageComplete];
    this._resetMessage();
    if (upgraded) this._upgraded = true;
    if (typeof cb === 'function') cb.call(this);
  }

  _messageComplete() {
    this._finishMessage();
    return this._upgraded ? 'upgraded' : 'more';
  }
}

HTTPParser.REQUEST = 1;
HTTPParser.RESPONSE = 2;
HTTPParser.kOnMessageBegin = kOnMessageBegin;
HTTPParser.kOnHeaders = kOnHeaders;
HTTPParser.kOnHeadersComplete = kOnHeadersComplete;
HTTPParser.kOnBody = kOnBody;
HTTPParser.kOnMessageComplete = kOnMessageComplete;
HTTPParser.kOnExecute = kOnExecute;
HTTPParser.kOnTimeout = kOnTimeout;
HTTPParser.kLenientNone = 0;
HTTPParser.kLenientHeaders = 1 << 0;
HTTPParser.kLenientChunkedLength = 1 << 1;
HTTPParser.kLenientKeepAlive = 1 << 2;
HTTPParser.kLenientTransferEncoding = 1 << 3;
HTTPParser.kLenientVersion = 1 << 4;
HTTPParser.kLenientDataAfterClose = 1 << 5;
HTTPParser.kLenientOptionalLFAfterCR = 1 << 6;
HTTPParser.kLenientOptionalCRLFAfterChunk = 1 << 7;
HTTPParser.kLenientOptionalCRBeforeLF = 1 << 8;
HTTPParser.kLenientSpacesAfterChunkSize = 1 << 9;
HTTPParser.kLenientHeaderValueRelaxed = 1 << 10;
HTTPParser.kLenientAll = (1 << 11) - 1;

module.exports = { methods, allMethods, HTTPParser, ConnectionsList };
