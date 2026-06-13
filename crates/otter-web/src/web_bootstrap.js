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
  // Legacy code constants on both the constructor and the prototype.
  for (const [cname, value] of Object.entries(DOMEXCEPTION_CODES)) {
    const constName = cname
      .replace(/Error$/, '')
      .replace(/([a-z])([A-Z])/g, '$1_$2')
      .toUpperCase() + '_ERR';
  }
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

  // ---- URLSearchParams as a global (also exposed via the URL native class) ----
  if (typeof global.URLSearchParams === 'undefined' && typeof global.URL !== 'undefined') {
    try {
      const sp = new global.URL('http://x/').searchParams;
      if (sp && sp.constructor && sp.constructor.name === 'URLSearchParams') {
        def('URLSearchParams', sp.constructor);
      }
    } catch (_) { /* URL has no searchParams ctor to borrow */ }
  }
})(globalThis);
