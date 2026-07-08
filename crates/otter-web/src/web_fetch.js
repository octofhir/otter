'use strict';
// WHATWG Fetch classes (Headers, Request, Response) implemented in JS over the
// bootstrap intrinsics, mirroring the approach of `web_bootstrap.js`. These are
// the single implementation of the fetch classes: they replace the former
// host-side records so that instances carry real prototype chains, standard
// constructor signatures, body mixins, and iteration.
//
// Evaluated after `web_bootstrap.js` (needs TextEncoder/TextDecoder, DOMException,
// URLSearchParams, FormData, AbortSignal) and `web_streams.js` (needs
// ReadableStream for the `body` getter).
//
// Server integrations construct instances through the hidden
// `__otterFetchInternals` factory to skip constructor validation on the hot
// path; its contract is documented at the bottom of this file.

(function installFetchClasses(global) {
  'use strict';

  function def(name, value) {
    Object.defineProperty(global, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  function tagged(proto, tag) {
    Object.defineProperty(proto, Symbol.toStringTag, {
      value: tag,
      writable: false,
      enumerable: false,
      configurable: true,
    });
  }

  let sharedEncoder = null;
  let sharedDecoder = null;
  function utf8Encode(text) {
    if (sharedEncoder === null) sharedEncoder = new TextEncoder();
    return sharedEncoder.encode(text);
  }
  function utf8Decode(bytes) {
    if (sharedDecoder === null) sharedDecoder = new TextDecoder();
    return sharedDecoder.decode(bytes);
  }

  // ---- Headers (Fetch § 5.1) ----
  //
  // Internal representation: array of [lowercasedName, value] pairs in
  // insertion order. Iteration combines values per name and sorts
  // lexicographically, per spec.

  const kHeaderList = Symbol('headerList');
  const kGuard = Symbol('headersGuard');

  function isValidHeaderName(name) {
    if (name.length === 0) return false;
    for (let i = 0; i < name.length; i++) {
      const c = name.charCodeAt(i);
      const ok =
        c === 0x21 || (c >= 0x23 && c <= 0x27) || c === 0x2a || c === 0x2b ||
        c === 0x2d || c === 0x2e || (c >= 0x30 && c <= 0x39) ||
        (c >= 0x41 && c <= 0x5a) || c === 0x5e || c === 0x5f || c === 0x60 ||
        (c >= 0x61 && c <= 0x7a) || c === 0x7c || c === 0x7e;
      if (!ok) return false;
    }
    return true;
  }

  function normalizeHeaderValue(value) {
    // Strip leading/trailing HTTP whitespace (tab, LF, CR, space).
    let start = 0;
    let end = value.length;
    while (start < end) {
      const c = value.charCodeAt(start);
      if (c === 0x09 || c === 0x0a || c === 0x0d || c === 0x20) start++;
      else break;
    }
    while (end > start) {
      const c = value.charCodeAt(end - 1);
      if (c === 0x09 || c === 0x0a || c === 0x0d || c === 0x20) end--;
      else break;
    }
    return value.slice(start, end);
  }

  function isValidHeaderValue(value) {
    for (let i = 0; i < value.length; i++) {
      const c = value.charCodeAt(i);
      if (c === 0x00 || c === 0x0a || c === 0x0d) return false;
    }
    return true;
  }

  function headersCheckName(op, name) {
    if (!isValidHeaderName(name)) {
      throw new TypeError(`Headers.${op}: invalid header name "${name}"`);
    }
  }

  function headersAppend(headers, rawName, rawValue) {
    const name = String(rawName).toLowerCase();
    const value = normalizeHeaderValue(String(rawValue));
    headersCheckName('append', name);
    if (!isValidHeaderValue(value)) {
      throw new TypeError(`Headers.append: invalid header value for "${name}"`);
    }
    if (headers[kGuard] === 'immutable') {
      throw new TypeError('Headers.append: headers are immutable');
    }
    headers[kHeaderList].push([name, value]);
  }

  function fillHeaders(headers, init) {
    if (init == null) return;
    if (init instanceof Headers) {
      for (const pair of init[kHeaderList]) {
        headers[kHeaderList].push([pair[0], pair[1]]);
      }
      return;
    }
    if (Array.isArray(init) || typeof init[Symbol.iterator] === 'function') {
      for (const pair of init) {
        const arr = Array.from(pair);
        if (arr.length !== 2) {
          throw new TypeError('Headers: each init pair must be a [name, value] tuple');
        }
        headersAppend(headers, arr[0], arr[1]);
      }
      return;
    }
    if (typeof init === 'object') {
      for (const key of Object.keys(init)) {
        headersAppend(headers, key, init[key]);
      }
      return;
    }
    throw new TypeError('Headers: invalid init');
  }

  class Headers {
    constructor(init) {
      Object.defineProperty(this, kHeaderList, { value: [], enumerable: false });
      Object.defineProperty(this, kGuard, {
        value: 'none',
        enumerable: false,
        writable: true,
      });
      fillHeaders(this, init);
    }

    append(name, value) {
      headersAppend(this, name, value);
    }

    delete(name) {
      name = String(name).toLowerCase();
      headersCheckName('delete', name);
      if (this[kGuard] === 'immutable') {
        throw new TypeError('Headers.delete: headers are immutable');
      }
      const list = this[kHeaderList];
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i][0] === name) list.splice(i, 1);
      }
    }

    get(name) {
      name = String(name).toLowerCase();
      headersCheckName('get', name);
      const values = [];
      for (const pair of this[kHeaderList]) {
        if (pair[0] === name) values.push(pair[1]);
      }
      return values.length === 0 ? null : values.join(', ');
    }

    getSetCookie() {
      const values = [];
      for (const pair of this[kHeaderList]) {
        if (pair[0] === 'set-cookie') values.push(pair[1]);
      }
      return values;
    }

    has(name) {
      name = String(name).toLowerCase();
      headersCheckName('has', name);
      return this[kHeaderList].some((pair) => pair[0] === name);
    }

    set(name, value) {
      name = String(name).toLowerCase();
      value = normalizeHeaderValue(String(value));
      headersCheckName('set', name);
      if (!isValidHeaderValue(value)) {
        throw new TypeError(`Headers.set: invalid header value for "${name}"`);
      }
      if (this[kGuard] === 'immutable') {
        throw new TypeError('Headers.set: headers are immutable');
      }
      const list = this[kHeaderList];
      let placed = false;
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i][0] === name) {
          if (placed) list.splice(i, 1);
          else { list[i][1] = value; placed = true; }
        }
      }
      if (!placed) list.push([name, value]);
    }

    forEach(callback, thisArg) {
      for (const [name, value] of sortedCombinedEntries(this)) {
        callback.call(thisArg, value, name, this);
      }
    }

    *entries() { yield* sortedCombinedEntries(this); }
    *keys() { for (const pair of sortedCombinedEntries(this)) yield pair[0]; }
    *values() { for (const pair of sortedCombinedEntries(this)) yield pair[1]; }
    [Symbol.iterator]() { return this.entries(); }
  }
  tagged(Headers.prototype, 'Headers');

  // Combined, name-sorted snapshot per Fetch § 5.1 "sort and combine".
  // `set-cookie` values stay as separate entries, per spec.
  function sortedCombinedEntries(headers) {
    const combined = new Map();
    const cookies = [];
    for (const [name, value] of headers[kHeaderList]) {
      if (name === 'set-cookie') {
        cookies.push(value);
      } else if (combined.has(name)) {
        combined.set(name, combined.get(name) + ', ' + value);
      } else {
        combined.set(name, value);
      }
    }
    const names = Array.from(combined.keys());
    if (cookies.length > 0) names.push('set-cookie');
    names.sort();
    const out = [];
    for (const name of names) {
      if (name === 'set-cookie') {
        for (const value of cookies) out.push([name, value]);
      } else {
        out.push([name, combined.get(name)]);
      }
    }
    return out;
  }

  // ---- Body (Fetch § 5.2) ----
  //
  // Internal body state per Request/Response instance:
  //   kBodyText:  string | null  — body kept as a JS string (fast common case)
  //   kBodyBytes: Uint8Array | null — body kept as bytes
  //   kBodyStream: ReadableStream | null — streaming body (drained on read)
  // At most one of the three is non-null. kBodyUsed marks disturbance.

  const kBodyText = Symbol('bodyText');
  const kBodyBytes = Symbol('bodyBytes');
  const kBodyStream = Symbol('bodyStream');
  const kBodyUsed = Symbol('bodyUsed');
  const kHeaders = Symbol('headers');

  function initBodySlots(target) {
    Object.defineProperty(target, kBodyText, { value: null, writable: true, enumerable: false });
    Object.defineProperty(target, kBodyBytes, { value: null, writable: true, enumerable: false });
    Object.defineProperty(target, kBodyStream, { value: null, writable: true, enumerable: false });
    Object.defineProperty(target, kBodyUsed, { value: false, writable: true, enumerable: false });
  }

  // Extract a body + implicit content type from a BodyInit value.
  // Returns the content type to auto-apply, or null.
  function extractBody(target, body) {
    if (body === null || body === undefined) return null;
    if (typeof body === 'string') {
      target[kBodyText] = body;
      return 'text/plain;charset=UTF-8';
    }
    if (body instanceof URLSearchParams) {
      target[kBodyText] = body.toString();
      return 'application/x-www-form-urlencoded;charset=UTF-8';
    }
    if (body instanceof Uint8Array) {
      target[kBodyBytes] = body.slice();
      return null;
    }
    if (typeof ArrayBuffer !== 'undefined' && body instanceof ArrayBuffer) {
      target[kBodyBytes] = new Uint8Array(body.slice(0));
      return null;
    }
    if (ArrayBuffer.isView(body)) {
      target[kBodyBytes] = new Uint8Array(
        body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength));
      return null;
    }
    if (typeof global.Blob === 'function' && body instanceof global.Blob) {
      target[kBodyText] = body.text();
      const type = body.type;
      return type === '' ? null : type;
    }
    if (typeof global.FormData === 'function' && body instanceof global.FormData) {
      const encoded = encodeMultipart(body);
      target[kBodyBytes] = encoded.bytes;
      return encoded.type;
    }
    if (typeof global.ReadableStream === 'function' && body instanceof global.ReadableStream) {
      target[kBodyStream] = body;
      return null;
    }
    target[kBodyText] = String(body);
    return 'text/plain;charset=UTF-8';
  }

  function multipartBoundary() {
    let tail = '';
    for (let i = 0; i < 24; i++) {
      tail += 'abcdefghijklmnopqrstuvwxyz0123456789'[Math.floor(Math.random() * 36)];
    }
    return '----OtterFormBoundary' + tail;
  }

  function encodeMultipart(formData) {
    const boundary = multipartBoundary();
    let out = '';
    for (const [name, value] of formData) {
      out += `--${boundary}\r\n`;
      if (typeof global.Blob === 'function' && value instanceof global.Blob) {
        const filename = typeof value.name === 'string' ? value.name : 'blob';
        const type = value.type === '' ? 'application/octet-stream' : value.type;
        out += `Content-Disposition: form-data; name="${name}"; filename="${filename}"\r\n`;
        out += `Content-Type: ${type}\r\n\r\n`;
        out += value.text();
        out += '\r\n';
      } else {
        out += `Content-Disposition: form-data; name="${name}"\r\n\r\n`;
        out += `${value}\r\n`;
      }
    }
    out += `--${boundary}--\r\n`;
    return {
      bytes: utf8Encode(out),
      type: 'multipart/form-data; boundary=' + boundary,
    };
  }

  function markBodyUsed(target, op) {
    if (target[kBodyUsed]) {
      throw new TypeError(`${op}: body has already been consumed`);
    }
    if (target[kBodyText] !== null || target[kBodyBytes] !== null ||
        target[kBodyStream] !== null) {
      target[kBodyUsed] = true;
    }
  }

  async function collectBodyBytes(target) {
    if (target[kBodyText] !== null) return utf8Encode(target[kBodyText]);
    if (target[kBodyBytes] !== null) return target[kBodyBytes];
    if (target[kBodyStream] !== null) {
      const reader = target[kBodyStream].getReader();
      const chunks = [];
      let total = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        const chunk = value instanceof Uint8Array ? value : utf8Encode(String(value));
        chunks.push(chunk);
        total += chunk.byteLength;
      }
      const out = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) {
        out.set(chunk, offset);
        offset += chunk.byteLength;
      }
      return out;
    }
    return new Uint8Array(0);
  }

  async function collectBodyText(target) {
    if (target[kBodyText] !== null) return target[kBodyText];
    return utf8Decode(await collectBodyBytes(target));
  }

  function parseUrlEncodedForm(text) {
    const form = new global.FormData();
    for (const [name, value] of new URLSearchParams(text)) {
      form.append(name, value);
    }
    return form;
  }

  function parseMultipartForm(text, contentType) {
    const match = /boundary="?([^";]+)"?/i.exec(contentType);
    if (!match) {
      throw new TypeError('formData: multipart body without boundary');
    }
    const boundary = '--' + match[1];
    const form = new global.FormData();
    const parts = text.split(boundary);
    // parts[0] is the preamble, the last part is the "--\r\n" epilogue.
    for (let i = 1; i < parts.length - 1; i++) {
      let part = parts[i];
      if (part.startsWith('\r\n')) part = part.slice(2);
      if (part.endsWith('\r\n')) part = part.slice(0, -2);
      const headerEnd = part.indexOf('\r\n\r\n');
      if (headerEnd < 0) continue;
      const rawHeaders = part.slice(0, headerEnd);
      const value = part.slice(headerEnd + 4);
      const disposition = /content-disposition:[^\r\n]*name="([^"]*)"/i.exec(rawHeaders);
      if (!disposition) continue;
      const name = disposition[1];
      const filename = /filename="([^"]*)"/i.exec(rawHeaders);
      if (filename) {
        const typeMatch = /content-type:\s*([^\r\n]+)/i.exec(rawHeaders);
        const file = new global.File(value, filename[1], {
          type: typeMatch ? typeMatch[1].trim() : '',
        });
        form.append(name, file, filename[1]);
      } else {
        form.append(name, value);
      }
    }
    return form;
  }

  const bodyMixin = {
    get body() {
      if (this[kBodyStream] !== null) return this[kBodyStream];
      if (this[kBodyText] === null && this[kBodyBytes] === null) return null;
      // Wrap the buffered body in a one-chunk stream, replacing the buffered
      // slot so `bodyUsed` tracking follows the stream from here on.
      const bytes = this[kBodyText] !== null
        ? utf8Encode(this[kBodyText])
        : this[kBodyBytes];
      this[kBodyText] = null;
      this[kBodyBytes] = null;
      const used = this[kBodyUsed];
      const stream = new global.ReadableStream({
        start(controller) {
          controller.enqueue(bytes);
          controller.close();
        },
      });
      this[kBodyStream] = stream;
      if (used) this[kBodyUsed] = true;
      return stream;
    },

    get bodyUsed() {
      return this[kBodyUsed];
    },

    async arrayBuffer() {
      markBodyUsed(this, 'arrayBuffer');
      const bytes = await collectBodyBytes(this);
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    },

    async bytes() {
      markBodyUsed(this, 'bytes');
      return (await collectBodyBytes(this)).slice();
    },

    async blob() {
      markBodyUsed(this, 'blob');
      const text = await collectBodyText(this);
      const type = this[kHeaders].get('content-type') || '';
      return new global.Blob(text, type);
    },

    async formData() {
      markBodyUsed(this, 'formData');
      const contentType = this[kHeaders].get('content-type') || '';
      const text = await collectBodyText(this);
      if (/^multipart\/form-data/i.test(contentType)) {
        return parseMultipartForm(text, contentType);
      }
      if (/^application\/x-www-form-urlencoded/i.test(contentType)) {
        return parseUrlEncodedForm(text);
      }
      throw new TypeError(`formData: unsupported content type "${contentType}"`);
    },

    async json() {
      markBodyUsed(this, 'json');
      return JSON.parse(await collectBodyText(this));
    },

    async text() {
      markBodyUsed(this, 'text');
      return collectBodyText(this);
    },
  };

  function installBodyMixin(proto) {
    for (const key of Object.keys(Object.getOwnPropertyDescriptors(bodyMixin))) {
      Object.defineProperty(proto, key,
        Object.getOwnPropertyDescriptor(bodyMixin, key));
    }
  }

  // Copy the raw (uncombined) header list into a fresh Headers.
  function cloneHeaders(source) {
    const headers = new Headers();
    for (const pair of source[kHeaderList]) {
      headers[kHeaderList].push([pair[0], pair[1]]);
    }
    return headers;
  }

  function cloneBodyInto(source, target) {
    target[kBodyText] = source[kBodyText];
    target[kBodyBytes] = source[kBodyBytes] === null ? null : source[kBodyBytes].slice();
    if (source[kBodyStream] !== null) {
      const [a, b] = source[kBodyStream].tee();
      source[kBodyStream] = a;
      target[kBodyStream] = b;
    } else {
      target[kBodyStream] = null;
    }
    target[kBodyUsed] = false;
  }

  // ---- Request (Fetch § 5.3) ----

  const kUrl = Symbol('url');
  const kMethod = Symbol('method');
  const kSignal = Symbol('signal');

  const NORMALIZED_METHODS = ['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT'];
  const FORBIDDEN_METHODS = ['CONNECT', 'TRACE', 'TRACK'];

  function normalizeMethod(rawMethod) {
    const method = String(rawMethod);
    const upper = method.toUpperCase();
    if (FORBIDDEN_METHODS.indexOf(upper) >= 0) {
      throw new TypeError(`Request: forbidden method "${method}"`);
    }
    return NORMALIZED_METHODS.indexOf(upper) >= 0 ? upper : method;
  }

  function requestUrlString(input) {
    if (typeof input === 'string') return input;
    if (input !== null && typeof input === 'object' &&
        typeof input.href === 'string') {
      return input.href;
    }
    return String(input);
  }

  class Request {
    constructor(input, init) {
      const options = init === undefined || init === null ? {} : init;
      initBodySlots(this);

      let method = 'GET';
      let headers = null;
      let signal = null;
      let bodySource = null;

      if (input instanceof Request) {
        Object.defineProperty(this, kUrl, { value: input[kUrl], enumerable: false });
        method = input[kMethod];
        headers = cloneHeaders(input[kHeaders]);
        signal = input[kSignal];
        if (options.body === undefined &&
            (input[kBodyText] !== null || input[kBodyBytes] !== null ||
             input[kBodyStream] !== null)) {
          if (input[kBodyUsed]) {
            throw new TypeError('Request: input request body has already been consumed');
          }
          bodySource = input;
        }
      } else {
        Object.defineProperty(this, kUrl, {
          value: requestUrlString(input),
          enumerable: false,
        });
        headers = new Headers();
      }

      if (options.method !== undefined) method = normalizeMethod(options.method);
      if (options.headers !== undefined) {
        headers = new Headers();
        fillHeaders(headers, options.headers);
      }
      if (options.signal !== undefined && options.signal !== null) {
        signal = options.signal;
      }

      Object.defineProperty(this, kMethod, { value: method, enumerable: false });
      Object.defineProperty(this, kHeaders, { value: headers, enumerable: false });
      Object.defineProperty(this, kSignal, { value: signal, enumerable: false });

      if (bodySource !== null) {
        cloneBodyInto(bodySource, this);
        bodySource[kBodyUsed] = true;
      } else if (options.body !== undefined && options.body !== null) {
        if (method === 'GET' || method === 'HEAD') {
          throw new TypeError(`Request: ${method} request cannot have a body`);
        }
        const impliedType = extractBody(this, options.body);
        if (impliedType !== null && !headers.has('content-type')) {
          headers.set('content-type', impliedType);
        }
      }
    }

    get url() { return this[kUrl]; }
    get method() { return this[kMethod]; }
    get headers() { return this[kHeaders]; }
    get signal() { return this[kSignal]; }
    get cache() { return 'default'; }
    get credentials() { return 'same-origin'; }
    get destination() { return ''; }
    get integrity() { return ''; }
    get keepalive() { return false; }
    get mode() { return 'cors'; }
    get redirect() { return 'follow'; }
    get referrer() { return 'about:client'; }
    get referrerPolicy() { return ''; }

    clone() {
      if (this[kBodyUsed]) {
        throw new TypeError('Request.clone: body has already been consumed');
      }
      const cloned = Object.create(Request.prototype);
      initBodySlots(cloned);
      Object.defineProperty(cloned, kUrl, { value: this[kUrl], enumerable: false });
      Object.defineProperty(cloned, kMethod, { value: this[kMethod], enumerable: false });
      Object.defineProperty(cloned, kHeaders, {
        value: cloneHeaders(this[kHeaders]),
        enumerable: false,
      });
      Object.defineProperty(cloned, kSignal, { value: this[kSignal], enumerable: false });
      cloneBodyInto(this, cloned);
      return cloned;
    }
  }
  installBodyMixin(Request.prototype);
  tagged(Request.prototype, 'Request');

  // ---- Response (Fetch § 5.4) ----

  const kStatus = Symbol('status');
  const kStatusText = Symbol('statusText');
  const kResponseType = Symbol('responseType');
  const kResponseUrl = Symbol('responseUrl');

  const REDIRECT_STATUSES = [301, 302, 303, 307, 308];

  function initResponse(response, status, statusText, headers, type) {
    initBodySlots(response);
    Object.defineProperty(response, kStatus, { value: status, enumerable: false });
    Object.defineProperty(response, kStatusText, { value: statusText, enumerable: false });
    Object.defineProperty(response, kHeaders, { value: headers, enumerable: false });
    Object.defineProperty(response, kResponseType, {
      value: type,
      writable: true,
      enumerable: false,
    });
    Object.defineProperty(response, kResponseUrl, {
      value: '',
      writable: true,
      enumerable: false,
    });
  }

  function responseStatus(init) {
    if (init === undefined || init === null || init.status === undefined) return 200;
    const status = Number(init.status);
    if (!Number.isInteger(status) || status < 200 || status > 599) {
      throw new RangeError(`Response: invalid status ${init.status}`);
    }
    return status;
  }

  function responseStatusText(init) {
    if (init === undefined || init === null || init.statusText === undefined) return '';
    return String(init.statusText);
  }

  class Response {
    constructor(body, init) {
      const headers = new Headers();
      if (init !== undefined && init !== null && init.headers !== undefined) {
        fillHeaders(headers, init.headers);
      }
      initResponse(this, responseStatus(init), responseStatusText(init), headers, 'default');
      if (body !== undefined && body !== null) {
        const status = this[kStatus];
        if (status === 204 || status === 205 || status === 304) {
          throw new TypeError(`Response: status ${status} cannot have a body`);
        }
        const impliedType = extractBody(this, body);
        if (impliedType !== null && !headers.has('content-type')) {
          headers.set('content-type', impliedType);
        }
      }
    }

    static error() {
      const response = Object.create(Response.prototype);
      initResponse(response, 0, '', new Headers(), 'error');
      response[kHeaders][kGuard] = 'immutable';
      return response;
    }

    static json(data, init) {
      const text = JSON.stringify(data);
      if (text === undefined) {
        throw new TypeError('Response.json: data is not JSON-serializable');
      }
      const headers = new Headers();
      if (init !== undefined && init !== null && init.headers !== undefined) {
        fillHeaders(headers, init.headers);
      }
      if (!headers.has('content-type')) {
        headers.set('content-type', 'application/json');
      }
      const response = Object.create(Response.prototype);
      initResponse(response, responseStatus(init), responseStatusText(init), headers, 'default');
      response[kBodyText] = text;
      return response;
    }

    static redirect(url, status) {
      const parsed = typeof url === 'object' && url !== null &&
        typeof url.href === 'string' ? url.href : String(url);
      const redirectStatus = status === undefined ? 302 : Number(status);
      if (REDIRECT_STATUSES.indexOf(redirectStatus) < 0) {
        throw new RangeError(`Response.redirect: invalid redirect status ${status}`);
      }
      const headers = new Headers();
      headers.set('location', parsed);
      const response = Object.create(Response.prototype);
      initResponse(response, redirectStatus, '', headers, 'default');
      return response;
    }

    get ok() { return this[kStatus] >= 200 && this[kStatus] <= 299; }
    get status() { return this[kStatus]; }
    get statusText() { return this[kStatusText]; }
    get headers() { return this[kHeaders]; }
    get redirected() { return false; }
    get type() { return this[kResponseType]; }
    get url() { return this[kResponseUrl]; }

    clone() {
      if (this[kBodyUsed]) {
        throw new TypeError('Response.clone: body has already been consumed');
      }
      const cloned = Object.create(Response.prototype);
      initResponse(cloned, this[kStatus], this[kStatusText],
        cloneHeaders(this[kHeaders]), this[kResponseType]);
      cloned[kResponseUrl] = this[kResponseUrl];
      cloneBodyInto(this, cloned);
      return cloned;
    }
  }
  installBodyMixin(Response.prototype);
  tagged(Response.prototype, 'Response');

  def('Headers', Headers);
  def('Request', Request);
  def('Response', Response);

  // ---- Server integration factory ----
  //
  // Native server glue builds Requests and unpacks Responses through this
  // hidden surface so the per-request hot path skips constructor validation.
  // Contract:
  //   makeRequest(method, url, flatHeaders, body)
  //     flatHeaders: [name0, value0, name1, value1, ...] (names pre-lowercased)
  //     body: string | Uint8Array | null
  //   responseParts(response) -> [status, flatHeaders, body]
  //     body: string | Uint8Array | ReadableStream | null; stream bodies must
  //     be drained by the caller via collectStream(stream).
  Object.defineProperty(global, '__otterFetchInternals', {
    value: Object.freeze({
      makeRequest(method, url, flatHeaders, body) {
        const request = Object.create(Request.prototype);
        initBodySlots(request);
        const headers = new Headers();
        const list = headers[kHeaderList];
        for (let i = 0; i + 1 < flatHeaders.length; i += 2) {
          list.push([flatHeaders[i], flatHeaders[i + 1]]);
        }
        Object.defineProperty(request, kUrl, { value: url, enumerable: false });
        Object.defineProperty(request, kMethod, { value: method, enumerable: false });
        Object.defineProperty(request, kHeaders, { value: headers, enumerable: false });
        Object.defineProperty(request, kSignal, { value: null, enumerable: false });
        if (body !== null && body !== undefined) {
          if (typeof body === 'string') request[kBodyText] = body;
          else request[kBodyBytes] = body;
        }
        return request;
      },
      // The private slot symbols, handed to the native server so it can read a
      // Response's status/headers/body directly in Rust instead of round-tripping
      // through `responseParts` + intermediate arrays. Exposed only on this
      // hidden internals object, so user code never gains slot access.
      slots: Object.freeze({
        status: kStatus,
        statusText: kStatusText,
        headers: kHeaders,
        headerList: kHeaderList,
        bodyText: kBodyText,
        bodyBytes: kBodyBytes,
        bodyStream: kBodyStream,
        bodyUsed: kBodyUsed,
      }),
      responseParts(response) {
        if (!(response instanceof Response)) return null;
        // Sorted-and-combined view (Fetch § 5.1), matching Headers iteration:
        // deterministic wire output regardless of init/default insert order.
        const flat = [];
        for (const [name, value] of sortedCombinedEntries(response[kHeaders])) {
          flat.push(name, value);
        }
        let body = null;
        if (response[kBodyText] !== null) body = response[kBodyText];
        else if (response[kBodyBytes] !== null) body = response[kBodyBytes];
        else if (response[kBodyStream] !== null) body = response[kBodyStream];
        response[kBodyUsed] = true;
        return [response[kStatus], response[kStatusText], flat, body];
      },
      async collectStream(stream) {
        const reader = stream.getReader();
        const chunks = [];
        let total = 0;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          const chunk = value instanceof Uint8Array ? value : utf8Encode(String(value));
          chunks.push(chunk);
          total += chunk.byteLength;
        }
        const out = new Uint8Array(total);
        let offset = 0;
        for (const chunk of chunks) {
          out.set(chunk, offset);
          offset += chunk.byteLength;
        }
        return out;
      },
    }),
    writable: false,
    enumerable: false,
    configurable: true,
  });
})(globalThis);
