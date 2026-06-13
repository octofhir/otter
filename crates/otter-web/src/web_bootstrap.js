'use strict';
// Pure-JS Web Platform globals, installed once at runtime bootstrap (eval'd by
// `web_globals_installer`). These APIs are naturally expressed in JS over
// existing intrinsics (Uint8Array, Error, Map, Symbol) — the same approach
// Node/Deno take in their internal JS. Native host classes (URL, Headers, Blob,
// Request, Response) are installed separately via `WEB_API_CLASSES`.
//
// Everything defined here is attached to `globalThis` with
// { writable: true, enumerable: false, configurable: true } to match the
// default attributes of platform globals.

(function installWebGlobals(global) {
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

  // ---- DOMException (§ WebIDL) ----
  const DOMEXCEPTION_CODES = {
    IndexSizeError: 1,
    HierarchyRequestError: 3,
    WrongDocumentError: 4,
    InvalidCharacterError: 5,
    NoModificationAllowedError: 7,
    NotFoundError: 8,
    NotSupportedError: 9,
    InUseAttributeError: 10,
    InvalidStateError: 11,
    SyntaxError: 12,
    InvalidModificationError: 13,
    NamespaceError: 14,
    InvalidAccessError: 15,
    SecurityError: 18,
    NetworkError: 19,
    AbortError: 20,
    URLMismatchError: 21,
    QuotaExceededError: 22,
    TimeoutError: 23,
    InvalidNodeTypeError: 24,
    DataCloneError: 25,
  };

  class DOMException extends Error {
    constructor(message = '', name = 'Error') {
      super(typeof message === 'undefined' ? '' : String(message));
      Object.defineProperty(this, 'name', {
        value: String(name),
        writable: true,
        enumerable: false,
        configurable: true,
      });
    }

    get code() {
      return DOMEXCEPTION_CODES[this.name] || 0;
    }
  }
  tagged(DOMException.prototype, 'DOMException');
  const LEGACY_CONSTS = {
    INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
    WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
    NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
    INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
    INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
    VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18,
    NETWORK_ERR: 19, ABORT_ERR: 20, URL_MISMATCH_ERR: 21,
    QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23, INVALID_NODE_TYPE_ERR: 24,
    DATA_CLONE_ERR: 25,
  };
  for (const [k, v] of Object.entries(LEGACY_CONSTS)) {
    Object.defineProperty(DOMException, k, { value: v, enumerable: true });
    Object.defineProperty(DOMException.prototype, k, { value: v, enumerable: true });
  }
  def('DOMException', DOMException);

  // ---- Event / CustomEvent (DOM § Events) ----
  const kStop = Symbol('stopPropagation');
  const kStopImmediate = Symbol('stopImmediate');
  const kTarget = Symbol('target');
  const kDispatch = Symbol('dispatching');

  function invalidArgTypeHelper(input) {
    if (input == null) return ` Received ${input}`;
    if (typeof input === 'function') return ` Received function ${input.name}`;
    if (typeof input === 'object') {
      if (input.constructor && input.constructor.name) {
        return ` Received an instance of ${input.constructor.name}`;
      }
      return ` Received ${Object.prototype.toString.call(input)}`;
    }
    if (typeof input === 'string') return ` Received type string ('${input}')`;
    return ` Received type ${typeof input} (${String(input)})`;
  }

  function validateEventOptions(options) {
    // Node validates `options` is an object (null/undefined allowed = no opts).
    if (options !== undefined && (typeof options !== 'object' || options === null) &&
        typeof options !== 'function') {
      const err = new TypeError(
        'The "options" argument must be of type object.' + invalidArgTypeHelper(options));
      err.code = 'ERR_INVALID_ARG_TYPE';
      throw err;
    }
    return options || {};
  }

  class Event {
    constructor(type, options = {}) {
      if (arguments.length === 0) {
        throw new TypeError("Failed to construct 'Event': 1 argument required");
      }
      const opts = validateEventOptions(options);
      // ToString — throws on Symbol, matching Node.
      this.type = `${type}`;
      this.bubbles = Boolean(opts.bubbles);
      this.cancelable = Boolean(opts.cancelable);
      this.composed = Boolean(opts.composed);
      this.defaultPrevented = false;
      this.eventPhase = 0;
      this.timeStamp = Date.now();
      this[kTarget] = null;
      this[kStop] = false;
      this[kStopImmediate] = false;
      this[kDispatch] = false;
      this.currentTarget = null;
      this.isTrusted = false;
    }

    get target() { return this[kTarget]; }
    get srcElement() { return this[kTarget]; }

    get cancelBubble() { return this[kStop]; }
    set cancelBubble(value) { if (value) this[kStop] = true; }

    get returnValue() { return !this.defaultPrevented; }
    set returnValue(value) { if (!value && this.cancelable) this.defaultPrevented = true; }

    preventDefault() {
      if (this.cancelable) {
        this.defaultPrevented = true;
      }
    }

    stopPropagation() { this[kStop] = true; }
    stopImmediatePropagation() { this[kStop] = true; this[kStopImmediate] = true; }

    composedPath() { return this[kDispatch] && this[kTarget] ? [this[kTarget]] : []; }
  }
  Event.NONE = 0;
  Event.CAPTURING_PHASE = 1;
  Event.AT_TARGET = 2;
  Event.BUBBLING_PHASE = 3;
  tagged(Event.prototype, 'Event');
  // `util.inspect.custom`: at negative depth show just the constructor name
  // (matches Node); otherwise fall through to default object formatting.
  Object.defineProperty(Event.prototype, Symbol.for('nodejs.util.inspect.custom'), {
    value: function inspectEvent(depth) {
      if (typeof depth === 'number' && depth < 0) return this.constructor.name;
      return this;
    },
    writable: true,
    enumerable: false,
    configurable: true,
  });
  def('Event', Event);

  class CustomEvent extends Event {
    constructor(type, options = {}) {
      if (arguments.length === 0) {
        throw new TypeError("Failed to construct 'CustomEvent': 1 argument required");
      }
      super(type, options);
      const opts = validateEventOptions(options);
      Object.defineProperty(this, 'detail', {
        value: 'detail' in opts ? opts.detail : null,
        writable: false,
        enumerable: true,
        configurable: true,
      });
    }
  }
  tagged(CustomEvent.prototype, 'CustomEvent');
  def('CustomEvent', CustomEvent);

  // ---- EventTarget (DOM § EventTarget) ----
  const kListeners = Symbol('listeners');

  function normalizeOptions(options) {
    if (typeof options === 'boolean') return { capture: options, once: false, passive: false };
    const o = options || {};
    return { capture: Boolean(o.capture), once: Boolean(o.once), passive: Boolean(o.passive) };
  }

  class EventTarget {
    constructor() {
      Object.defineProperty(this, kListeners, {
        value: new Map(),
        enumerable: false,
        writable: false,
        configurable: false,
      });
    }

    addEventListener(type, listener, options) {
      if (listener == null) return;
      if (typeof listener !== 'function' && typeof listener.handleEvent !== 'function') {
        throw new TypeError('The "listener" argument must be of type function or an object with a handleEvent method');
      }
      type = String(type);
      const { capture, once, passive } = normalizeOptions(options);
      let list = this[kListeners].get(type);
      if (!list) { list = []; this[kListeners].set(type, list); }
      for (const entry of list) {
        if (entry.listener === listener && entry.capture === capture) return;
      }
      list.push({ listener, capture, once, passive });
    }

    removeEventListener(type, listener, options) {
      if (listener == null) return;
      type = String(type);
      const { capture } = normalizeOptions(options);
      const list = this[kListeners].get(type);
      if (!list) return;
      for (let i = 0; i < list.length; i++) {
        if (list[i].listener === listener && list[i].capture === capture) {
          list.splice(i, 1);
          break;
        }
      }
    }

    dispatchEvent(event) {
      if (!(event instanceof Event)) {
        throw new TypeError('The "event" argument must be an instance of Event');
      }
      if (event[kDispatch]) {
        throw new DOMException('The event is already being dispatched', 'InvalidStateError');
      }
      event[kDispatch] = true;
      event[kStop] = false;
      event[kStopImmediate] = false;
      event.defaultPrevented = false;
      event[kTarget] = this;
      event.currentTarget = this;
      event.eventPhase = Event.AT_TARGET;
      const list = this[kListeners].get(event.type);
      if (list) {
        for (const entry of list.slice()) {
          if (event[kStopImmediate]) break;
          if (entry.once) this.removeEventListener(event.type, entry.listener, entry.capture);
          const fn = typeof entry.listener === 'function'
            ? entry.listener
            : entry.listener.handleEvent;
          const thisArg = typeof entry.listener === 'function' ? this : entry.listener;
          try {
            fn.call(thisArg, event);
          } catch (err) {
            // Match Node: report the error but keep dispatching.
            if (typeof reportError === 'function') reportError(err);
            else Promise.reject(err);
          }
        }
      }
      event.currentTarget = null;
      event.eventPhase = Event.NONE;
      event[kDispatch] = false;
      return !event.defaultPrevented;
    }
  }
  tagged(EventTarget.prototype, 'EventTarget');
  def('EventTarget', EventTarget);

  // ---- performance (High Resolution Time, minimal) ----
  const timeOrigin = Date.now();
  class Performance {
    now() { return Date.now() - timeOrigin; }
    get timeOrigin() { return timeOrigin; }
    toJSON() { return { timeOrigin }; }
  }
  tagged(Performance.prototype, 'Performance');
  def('performance', new Performance());

  // ---- TextEncoder / TextDecoder (Encoding §) ----
  // windows-1252 high range (0x80-0x9F) → Unicode; 0xA0-0xFF map 1:1.
  const CP1252_HIGH = [
    0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021,
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D, 0x8F,
    0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
  ];

  function labelToEncoding(label) {
    const l = String(label == null ? 'utf-8' : label).trim().toLowerCase();
    switch (l) {
      case 'utf-8': case 'utf8': case 'unicode-1-1-utf-8': return 'utf-8';
      case 'windows-1252': case 'cp1252': case 'ansi_x3.4-1968': case 'ascii':
      case 'latin1': case 'iso-8859-1': case 'l1': case 'cp819': return 'windows-1252';
      case 'utf-16le': case 'utf-16': case 'ucs-2': case 'unicodefeff': return 'utf-16le';
      case 'utf-16be': return 'utf-16be';
      default: return null;
    }
  }

  class TextEncoder {
    constructor() {}
    get encoding() { return 'utf-8'; }
    encode(input = '') {
      const str = String(input);
      const out = [];
      for (let i = 0; i < str.length; i++) {
        let c = str.charCodeAt(i);
        if (c >= 0xD800 && c <= 0xDBFF && i + 1 < str.length) {
          const c2 = str.charCodeAt(i + 1);
          if (c2 >= 0xDC00 && c2 <= 0xDFFF) {
            c = 0x10000 + ((c - 0xD800) << 10) + (c2 - 0xDC00);
            i++;
          }
        }
        if (c < 0x80) out.push(c);
        else if (c < 0x800) { out.push(0xC0 | (c >> 6), 0x80 | (c & 0x3F)); }
        else if (c < 0x10000) {
          if (c >= 0xD800 && c <= 0xDFFF) c = 0xFFFD;
          out.push(0xE0 | (c >> 12), 0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F));
        } else {
          out.push(0xF0 | (c >> 18), 0x80 | ((c >> 12) & 0x3F),
                   0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F));
        }
      }
      return Uint8Array.from(out);
    }
    encodeInto(source, dest) {
      const encoded = this.encode(source);
      const n = Math.min(encoded.length, dest.length);
      dest.set(encoded.subarray(0, n));
      // `read` is an approximation (assumes 1:1 below the truncation point).
      return { read: n === encoded.length ? String(source).length : n, written: n };
    }
  }
  tagged(TextEncoder.prototype, 'TextEncoder');
  def('TextEncoder', TextEncoder);

  function bytesOf(input) {
    if (input == null) return new Uint8Array(0);
    if (input instanceof Uint8Array) return input;
    if (ArrayBuffer.isView(input)) {
      return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    }
    if (input instanceof ArrayBuffer) return new Uint8Array(input);
    throw new TypeError('The provided value is not of type ArrayBuffer or ArrayBufferView');
  }

  function decodeUtf8(bytes, fatal) {
    let out = '';
    let i = 0;
    const n = bytes.length;
    while (i < n) {
      const b0 = bytes[i++];
      if (b0 < 0x80) { out += String.fromCharCode(b0); continue; }
      let cp, extra, min;
      if ((b0 & 0xE0) === 0xC0) { cp = b0 & 0x1F; extra = 1; min = 0x80; }
      else if ((b0 & 0xF0) === 0xE0) { cp = b0 & 0x0F; extra = 2; min = 0x800; }
      else if ((b0 & 0xF8) === 0xF0) { cp = b0 & 0x07; extra = 3; min = 0x10000; }
      else { if (fatal) throw new TypeError('The encoded data was not valid'); out += '�'; continue; }
      let ok = true;
      for (let k = 0; k < extra; k++) {
        if (i >= n) { ok = false; break; }
        const b = bytes[i];
        if ((b & 0xC0) !== 0x80) { ok = false; break; }
        cp = (cp << 6) | (b & 0x3F);
        i++;
      }
      if (!ok || cp < min || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) {
        if (fatal) throw new TypeError('The encoded data was not valid');
        out += '�';
        continue;
      }
      if (cp > 0xFFFF) {
        cp -= 0x10000;
        out += String.fromCharCode(0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF));
      } else {
        out += String.fromCharCode(cp);
      }
    }
    return out;
  }

  function decodeCp1252(bytes) {
    let out = '';
    for (let i = 0; i < bytes.length; i++) {
      const b = bytes[i];
      if (b < 0x80 || b >= 0xA0) out += String.fromCharCode(b);
      else out += String.fromCharCode(CP1252_HIGH[b - 0x80]);
    }
    return out;
  }

  function decodeUtf16(bytes, littleEndian) {
    let out = '';
    for (let i = 0; i + 1 < bytes.length; i += 2) {
      const unit = littleEndian
        ? bytes[i] | (bytes[i + 1] << 8)
        : (bytes[i] << 8) | bytes[i + 1];
      out += String.fromCharCode(unit);
    }
    return out;
  }

  class TextDecoder {
    constructor(label = 'utf-8', options = {}) {
      const enc = labelToEncoding(label);
      if (enc == null) {
        throw new RangeError(`The "${label}" encoding is not supported`);
      }
      const opts = options || {};
      Object.defineProperty(this, '_encoding', { value: enc, enumerable: false });
      Object.defineProperty(this, '_fatal', { value: Boolean(opts.fatal), enumerable: false });
      Object.defineProperty(this, '_ignoreBOM', { value: Boolean(opts.ignoreBOM), enumerable: false });
    }
    get encoding() { return this._encoding; }
    get fatal() { return this._fatal; }
    get ignoreBOM() { return this._ignoreBOM; }
    decode(input, options) {
      let bytes = bytesOf(input);
      // BOM handling (default: strip).
      if (!this._ignoreBOM) {
        if (this._encoding === 'utf-8' && bytes.length >= 3 &&
            bytes[0] === 0xEF && bytes[1] === 0xBB && bytes[2] === 0xBF) {
          bytes = bytes.subarray(3);
        } else if (this._encoding === 'utf-16le' && bytes.length >= 2 &&
                   bytes[0] === 0xFF && bytes[1] === 0xFE) {
          bytes = bytes.subarray(2);
        } else if (this._encoding === 'utf-16be' && bytes.length >= 2 &&
                   bytes[0] === 0xFE && bytes[1] === 0xFF) {
          bytes = bytes.subarray(2);
        }
      }
      switch (this._encoding) {
        case 'utf-8': return decodeUtf8(bytes, this._fatal);
        case 'windows-1252': return decodeCp1252(bytes);
        case 'utf-16le': return decodeUtf16(bytes, true);
        case 'utf-16be': return decodeUtf16(bytes, false);
        default: return decodeUtf8(bytes, this._fatal);
      }
    }
  }
  tagged(TextDecoder.prototype, 'TextDecoder');
  def('TextDecoder', TextDecoder);

  // ---- AbortController / AbortSignal (DOM § Aborting) ----
  const kAbortInternal = Symbol('AbortSignalInternal');
  const kAborted = Symbol('aborted');
  const kReason = Symbol('reason');
  const kOnabort = Symbol('onabort');
  const kSignal = Symbol('signal');

  function makeAbortReason(reason) {
    return reason !== undefined
      ? reason
      : new DOMException('This operation was aborted', 'AbortError');
  }

  function runAbort(signal, reason) {
    if (signal[kAborted]) return;
    signal[kAborted] = true;
    signal[kReason] = makeAbortReason(reason);
    signal.dispatchEvent(new Event('abort'));
  }

  class AbortSignal extends EventTarget {
    constructor(key) {
      if (key !== kAbortInternal) {
        throw new TypeError('Illegal constructor');
      }
      super();
      this[kAborted] = false;
      this[kReason] = undefined;
      this[kOnabort] = null;
    }
    get aborted() { return this[kAborted]; }
    get reason() { return this[kReason]; }
    get onabort() { return this[kOnabort]; }
    set onabort(fn) {
      if (this[kOnabort]) this.removeEventListener('abort', this[kOnabort]);
      this[kOnabort] = typeof fn === 'function' ? fn : null;
      if (this[kOnabort]) this.addEventListener('abort', this[kOnabort]);
    }
    throwIfAborted() { if (this[kAborted]) throw this[kReason]; }
    static abort(reason) {
      const s = new AbortSignal(kAbortInternal);
      s[kAborted] = true;
      s[kReason] = makeAbortReason(reason);
      return s;
    }
    static timeout(ms) {
      const s = new AbortSignal(kAbortInternal);
      setTimeout(() => runAbort(s, new DOMException('The operation timed out', 'TimeoutError')),
        Number(ms));
      return s;
    }
    static any(signals) {
      const result = new AbortSignal(kAbortInternal);
      for (const sig of signals) {
        if (sig.aborted) { runAbort(result, sig.reason); break; }
        sig.addEventListener('abort', () => runAbort(result, sig.reason), { once: true });
      }
      return result;
    }
  }
  tagged(AbortSignal.prototype, 'AbortSignal');
  def('AbortSignal', AbortSignal);

  class AbortController {
    constructor() {
      Object.defineProperty(this, kSignal, {
        value: new AbortSignal(kAbortInternal),
        enumerable: false,
      });
    }
    get signal() { return this[kSignal]; }
    abort(reason) { runAbort(this[kSignal], reason); }
  }
  tagged(AbortController.prototype, 'AbortController');
  def('AbortController', AbortController);

  // ---- Event subclasses ----
  class MessageEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      const o = options || {};
      this.data = 'data' in o ? o.data : null;
      this.origin = o.origin !== undefined ? String(o.origin) : '';
      this.lastEventId = o.lastEventId !== undefined ? String(o.lastEventId) : '';
      this.source = o.source !== undefined ? o.source : null;
      this.ports = o.ports !== undefined ? o.ports : [];
    }
  }
  tagged(MessageEvent.prototype, 'MessageEvent');
  def('MessageEvent', MessageEvent);

  class CloseEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      const o = options || {};
      this.wasClean = Boolean(o.wasClean);
      this.code = o.code !== undefined ? Number(o.code) : 0;
      this.reason = o.reason !== undefined ? String(o.reason) : '';
    }
  }
  tagged(CloseEvent.prototype, 'CloseEvent');
  def('CloseEvent', CloseEvent);

  class ErrorEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      const o = options || {};
      this.message = o.message !== undefined ? String(o.message) : '';
      this.filename = o.filename !== undefined ? String(o.filename) : '';
      this.lineno = o.lineno !== undefined ? Number(o.lineno) : 0;
      this.colno = o.colno !== undefined ? Number(o.colno) : 0;
      this.error = 'error' in o ? o.error : undefined;
    }
  }
  tagged(ErrorEvent.prototype, 'ErrorEvent');
  def('ErrorEvent', ErrorEvent);

  class ProgressEvent extends Event {
    constructor(type, options = {}) {
      super(type, options);
      const o = options || {};
      this.lengthComputable = Boolean(o.lengthComputable);
      this.loaded = o.loaded !== undefined ? Number(o.loaded) : 0;
      this.total = o.total !== undefined ? Number(o.total) : 0;
    }
  }
  tagged(ProgressEvent.prototype, 'ProgressEvent');
  def('ProgressEvent', ProgressEvent);

  // ---- MessageChannel / MessagePort (HTML § channel messaging) ----
  const kPortInternal = Symbol('MessagePortInternal');
  const kOther = Symbol('otherPort');
  const kStarted = Symbol('started');
  const kQueue = Symbol('queue');
  const kOnmessage = Symbol('onmessage');

  function deliver(port, data) {
    port.dispatchEvent(new MessageEvent('message', { data }));
  }

  class MessagePort extends EventTarget {
    constructor(key) {
      if (key !== kPortInternal) throw new TypeError('Illegal constructor');
      super();
      this[kOther] = null;
      this[kStarted] = false;
      this[kQueue] = [];
      this[kOnmessage] = null;
    }

    get onmessage() { return this[kOnmessage]; }
    set onmessage(fn) {
      if (this[kOnmessage]) this.removeEventListener('message', this[kOnmessage]);
      this[kOnmessage] = typeof fn === 'function' ? fn : null;
      if (this[kOnmessage]) {
        this.addEventListener('message', this[kOnmessage]);
        this.start();
      }
    }

    postMessage(message) {
      const other = this[kOther];
      if (!other) return;
      queueMicrotask(() => {
        if (other[kStarted]) deliver(other, message);
        else other[kQueue].push(message);
      });
    }

    start() {
      if (this[kStarted]) return;
      this[kStarted] = true;
      const queued = this[kQueue];
      this[kQueue] = [];
      for (const data of queued) queueMicrotask(() => deliver(this, data));
    }

    close() { this[kOther] = null; }
  }
  tagged(MessagePort.prototype, 'MessagePort');
  def('MessagePort', MessagePort);

  class MessageChannel {
    constructor() {
      const p1 = new MessagePort(kPortInternal);
      const p2 = new MessagePort(kPortInternal);
      p1[kOther] = p2;
      p2[kOther] = p1;
      this.port1 = p1;
      this.port2 = p2;
    }
  }
  tagged(MessageChannel.prototype, 'MessageChannel');
  def('MessageChannel', MessageChannel);


  // ---- URLSearchParams (WHATWG URL § application/x-www-form-urlencoded) ----
  const kList = Symbol('list');

  // form-urlencoded byte serializer: space -> '+', and percent-encode the rest
  // outside the unreserved set, over the UTF-8 bytes.
  function formEncode(str) {
    let out = '';
    const enc = new TextEncoder().encode(String(str));
    for (let i = 0; i < enc.length; i++) {
      const b = enc[i];
      if (b === 0x20) out += '+';
      else if (
        (b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x5A) ||
        (b >= 0x61 && b <= 0x7A) || b === 0x2A || b === 0x2D ||
        b === 0x2E || b === 0x5F
      ) {
        out += String.fromCharCode(b);
      } else {
        out += '%' + b.toString(16).toUpperCase().padStart(2, '0');
      }
    }
    return out;
  }

  function formDecode(str) {
    const replaced = String(str).replace(/\+/g, ' ');
    try { return decodeURIComponent(replaced); } catch (_) { return replaced; }
  }

  function parseQuery(input) {
    const list = [];
    let s = String(input);
    if (s.charCodeAt(0) === 0x3F) s = s.slice(1); // leading '?'
    if (s === '') return list;
    for (const pair of s.split('&')) {
      if (pair === '') continue;
      const eq = pair.indexOf('=');
      let name, value;
      if (eq < 0) { name = pair; value = ''; }
      else { name = pair.slice(0, eq); value = pair.slice(eq + 1); }
      list.push([formDecode(name), formDecode(value)]);
    }
    return list;
  }

  class URLSearchParams {
    constructor(init = '') {
      let list;
      if (init == null || init === '') {
        list = [];
      } else if (typeof init === 'string') {
        list = parseQuery(init);
      } else if (init instanceof URLSearchParams) {
        list = init[kList].map((p) => [p[0], p[1]]);
      } else if (typeof init[Symbol.iterator] === 'function') {
        list = [];
        for (const pair of init) {
          const arr = Array.from(pair);
          if (arr.length !== 2) {
            throw new TypeError('Each query pair must be an iterable [name, value] tuple');
          }
          list.push([String(arr[0]), String(arr[1])]);
        }
      } else if (typeof init === 'object') {
        list = Object.keys(init).map((k) => [k, String(init[k])]);
      } else {
        list = parseQuery(init);
      }
      Object.defineProperty(this, kList, { value: list, enumerable: false });
    }

    get size() { return this[kList].length; }

    append(name, value) { this[kList].push([String(name), String(value)]); }

    delete(name, value) {
      name = String(name);
      const list = this[kList];
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i][0] === name && (value === undefined || list[i][1] === String(value))) {
          list.splice(i, 1);
        }
      }
    }

    get(name) {
      name = String(name);
      for (const p of this[kList]) if (p[0] === name) return p[1];
      return null;
    }

    getAll(name) {
      name = String(name);
      return this[kList].filter((p) => p[0] === name).map((p) => p[1]);
    }

    has(name, value) {
      name = String(name);
      return this[kList].some(
        (p) => p[0] === name && (value === undefined || p[1] === String(value)));
    }

    set(name, value) {
      name = String(name);
      value = String(value);
      const list = this[kList];
      let found = false;
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i][0] === name) {
          if (found) list.splice(i, 1);
          else { list[i][1] = value; found = true; }
        }
      }
      if (!found) list.push([name, value]);
    }

    sort() { this[kList].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); }

    forEach(callback, thisArg) {
      for (const [name, value] of this[kList].slice()) {
        callback.call(thisArg, value, name, this);
      }
    }

    *entries() { for (const p of this[kList].slice()) yield [p[0], p[1]]; }
    *keys() { for (const p of this[kList].slice()) yield p[0]; }
    *values() { for (const p of this[kList].slice()) yield p[1]; }
    [Symbol.iterator]() { return this.entries(); }

    toString() {
      return this[kList].map((p) => `${formEncode(p[0])}=${formEncode(p[1])}`).join('&');
    }
  }
  tagged(URLSearchParams.prototype, 'URLSearchParams');
  def('URLSearchParams', URLSearchParams);

  // ---- FormData (XHR § FormData) ----
  const kEntries = Symbol('entries');

  function toFormValue(value, filename) {
    const isBlob = typeof global.Blob !== 'undefined' && value instanceof global.Blob;
    if (isBlob) {
      // A bare Blob appended to FormData becomes a File named "blob".
      const name = filename !== undefined ? String(filename)
        : (typeof value.name === 'string' ? value.name : 'blob');
      return { value, filename: name };
    }
    return { value: String(value), filename: undefined };
  }

  class FormData {
    constructor() {
      Object.defineProperty(this, kEntries, { value: [], enumerable: false });
    }

    append(name, value, filename) {
      const e = toFormValue(value, filename);
      this[kEntries].push([String(name), e.value, e.filename]);
    }

    set(name, value, filename) {
      name = String(name);
      const e = toFormValue(value, filename);
      const list = this[kEntries];
      let placed = false;
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i][0] === name) {
          if (placed) list.splice(i, 1);
          else { list[i] = [name, e.value, e.filename]; placed = true; }
        }
      }
      if (!placed) list.push([name, e.value, e.filename]);
    }

    get(name) {
      name = String(name);
      for (const e of this[kEntries]) if (e[0] === name) return e[1];
      return null;
    }

    getAll(name) {
      name = String(name);
      return this[kEntries].filter((e) => e[0] === name).map((e) => e[1]);
    }

    has(name) {
      name = String(name);
      return this[kEntries].some((e) => e[0] === name);
    }

    delete(name) {
      name = String(name);
      const list = this[kEntries];
      for (let i = list.length - 1; i >= 0; i--) if (list[i][0] === name) list.splice(i, 1);
    }

    forEach(callback, thisArg) {
      for (const e of this[kEntries].slice()) callback.call(thisArg, e[1], e[0], this);
    }

    *entries() { for (const e of this[kEntries].slice()) yield [e[0], e[1]]; }
    *keys() { for (const e of this[kEntries].slice()) yield e[0]; }
    *values() { for (const e of this[kEntries].slice()) yield e[1]; }
    [Symbol.iterator]() { return this.entries(); }
  }
  tagged(FormData.prototype, 'FormData');
  def('FormData', FormData);

  // ---- BroadcastChannel (HTML § Broadcasting, single agent) ----
  const channelRegistry = new Map(); // name -> Set<BroadcastChannel>
  const kChannelName = Symbol('channelName');
  const kClosed = Symbol('closed');
  const kBcOnmessage = Symbol('bcOnmessage');

  class BroadcastChannel extends EventTarget {
    constructor(name) {
      if (arguments.length === 0) {
        throw new TypeError("Failed to construct 'BroadcastChannel': 1 argument required");
      }
      super();
      this[kChannelName] = String(name);
      this[kClosed] = false;
      this[kBcOnmessage] = null;
      let peers = channelRegistry.get(this[kChannelName]);
      if (!peers) { peers = new Set(); channelRegistry.set(this[kChannelName], peers); }
      peers.add(this);
    }

    get name() { return this[kChannelName]; }

    get onmessage() { return this[kBcOnmessage]; }
    set onmessage(fn) {
      if (this[kBcOnmessage]) this.removeEventListener('message', this[kBcOnmessage]);
      this[kBcOnmessage] = typeof fn === 'function' ? fn : null;
      if (this[kBcOnmessage]) this.addEventListener('message', this[kBcOnmessage]);
    }

    postMessage(message) {
      if (this[kClosed]) throw new DOMException('Channel is closed', 'InvalidStateError');
      const data = structuredClone(message);
      const peers = channelRegistry.get(this[kChannelName]);
      if (!peers) return;
      for (const peer of peers) {
        if (peer === this || peer[kClosed]) continue;
        queueMicrotask(() => {
          if (!peer[kClosed]) peer.dispatchEvent(new MessageEvent('message', { data }));
        });
      }
    }

    close() {
      if (this[kClosed]) return;
      this[kClosed] = true;
      const peers = channelRegistry.get(this[kChannelName]);
      if (peers) {
        peers.delete(this);
        if (peers.size === 0) channelRegistry.delete(this[kChannelName]);
      }
    }
  }
  tagged(BroadcastChannel.prototype, 'BroadcastChannel');
  def('BroadcastChannel', BroadcastChannel);
})(globalThis);
