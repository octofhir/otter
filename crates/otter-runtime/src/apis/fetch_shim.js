(function (global) {
  function encodeUtf8(text) {
    const encoded = unescape(encodeURIComponent(text));
    const bytes = new Uint8Array(encoded.length);
    for (let i = 0; i < encoded.length; i += 1) {
      bytes[i] = encoded.charCodeAt(i);
    }
    return bytes;
  }

  function decodeUtf8(bytes) {
    let result = "";
    for (let i = 0; i < bytes.length; i += 1) {
      result += String.fromCharCode(bytes[i]);
    }
    return decodeURIComponent(escape(result));
  }

  class URLSearchParams {
    constructor(init = "") {
      this._pairs = [];
      if (init instanceof URLSearchParams) {
        init.forEach((value, key) => {
          this.append(key, value);
        });
      } else if (typeof init === "string") {
        init.replace(/^\?/, "").split("&").filter(Boolean).forEach((pair) => {
          const [key, value = ""] = pair.split("=");
          this.append(decodeURIComponent(key), decodeURIComponent(value));
        });
      } else if (init && typeof init === "object") {
        Object.keys(init).forEach((key) => {
          this.append(key, init[key]);
        });
      }
    }

    append(key, value) {
      this._pairs.push([String(key), String(value)]);
    }

    set(key, value) {
      this.delete(key);
      this.append(key, value);
    }

    get(key) {
      const entry = this._pairs.find(([k]) => k === String(key));
      return entry ? entry[1] : null;
    }

    getAll(key) {
      return this._pairs.filter(([k]) => k === String(key)).map(([, v]) => v);
    }

    has(key) {
      return this._pairs.some(([k]) => k === String(key));
    }

    delete(key) {
      const target = String(key);
      this._pairs = this._pairs.filter(([k]) => k !== target);
    }

    forEach(callback, thisArg) {
      this._pairs.forEach(([key, value]) => {
        callback.call(thisArg, value, key, this);
      });
    }

    entries() {
      return this._pairs[Symbol.iterator]();
    }

    keys() {
      return this._pairs.map(([key]) => key)[Symbol.iterator]();
    }

    values() {
      return this._pairs.map(([, value]) => value)[Symbol.iterator]();
    }

    toString() {
      return this._pairs
        .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`)
        .join("&");
    }

    [Symbol.iterator]() {
      return this.entries();
    }
  }

  class Headers {
    constructor(init = {}) {
      this._map = new Map();
      if (init instanceof Headers) {
        init.forEach((value, key) => {
          this.set(key, value);
        });
      } else if (Array.isArray(init)) {
        init.forEach(([key, value]) => {
          this.append(key, value);
        });
      } else if (init && typeof init === "object") {
        Object.keys(init).forEach((key) => {
          this.set(key, init[key]);
        });
      }
    }

    append(key, value) {
      const normalized = String(key).toLowerCase();
      const current = this._map.get(normalized);
      if (current) {
        this._map.set(normalized, `${current}, ${value}`);
      } else {
        this._map.set(normalized, String(value));
      }
    }

    set(key, value) {
      this._map.set(String(key).toLowerCase(), String(value));
    }

    get(key) {
      return this._map.get(String(key).toLowerCase()) ?? null;
    }

    has(key) {
      return this._map.has(String(key).toLowerCase());
    }

    delete(key) {
      return this._map.delete(String(key).toLowerCase());
    }

    forEach(callback, thisArg) {
      this._map.forEach((value, key) => {
        callback.call(thisArg, value, key, this);
      });
    }

    entries() {
      return this._map.entries();
    }

    keys() {
      return this._map.keys();
    }

    values() {
      return this._map.values();
    }

    [Symbol.iterator]() {
      return this._map.entries();
    }
  }

  class FormData {
    constructor() {
      this._entries = [];
    }

    append(key, value) {
      this._entries.push([String(key), String(value)]);
    }

    set(key, value) {
      this.delete(key);
      this.append(key, value);
    }

    get(key) {
      const entry = this._entries.find(([k]) => k === String(key));
      return entry ? entry[1] : null;
    }

    getAll(key) {
      return this._entries.filter(([k]) => k === String(key)).map(([, v]) => v);
    }

    has(key) {
      return this._entries.some(([k]) => k === String(key));
    }

    delete(key) {
      const target = String(key);
      this._entries = this._entries.filter(([k]) => k !== target);
    }

    forEach(callback, thisArg) {
      this._entries.forEach(([key, value]) => {
        callback.call(thisArg, value, key, this);
      });
    }

    entries() {
      return this._entries[Symbol.iterator]();
    }

    keys() {
      return this._entries.map(([key]) => key)[Symbol.iterator]();
    }

    values() {
      return this._entries.map(([, value]) => value)[Symbol.iterator]();
    }

    [Symbol.iterator]() {
      return this.entries();
    }

    toString() {
      return this._entries
        .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`)
        .join("&");
    }
  }

  class Blob {
    constructor(parts = [], options = {}) {
      this.type = options.type ? String(options.type).toLowerCase() : "";
      const chunks = [];
      let size = 0;
      parts.forEach((part) => {
        if (part instanceof Uint8Array) {
          chunks.push(part);
          size += part.byteLength;
        } else if (part instanceof ArrayBuffer) {
          const bytes = new Uint8Array(part);
          chunks.push(bytes);
          size += bytes.byteLength;
        } else {
          const bytes = encodeUtf8(String(part));
          chunks.push(bytes);
          size += bytes.byteLength;
        }
      });
      this._chunks = chunks;
      this.size = size;
    }

    async text() {
      const buffer = await this.arrayBuffer();
      return decodeUtf8(new Uint8Array(buffer));
    }

    async arrayBuffer() {
      const buffer = new Uint8Array(this.size);
      let offset = 0;
      this._chunks.forEach((chunk) => {
        buffer.set(chunk, offset);
        offset += chunk.byteLength;
      });
      return buffer.buffer;
    }
  }

  class BodyMixin {
    constructor(body) {
      this._body = body ?? "";
      this.bodyUsed = false;
      this.body = null;
    }

    async text() {
      this.bodyUsed = true;
      if (this._body instanceof Blob) {
        return this._body.text();
      }
      if (this._body instanceof Uint8Array) {
        return decodeUtf8(this._body);
      }
      if (this._body instanceof ArrayBuffer) {
        return decodeUtf8(new Uint8Array(this._body));
      }
      if (this._body instanceof URLSearchParams || this._body instanceof FormData) {
        return this._body.toString();
      }
      return typeof this._body === "string" ? this._body : String(this._body ?? "");
    }

    async json() {
      const text = await this.text();
      return JSON.parse(text);
    }

    async arrayBuffer() {
      this.bodyUsed = true;
      if (this._body instanceof Blob) {
        return this._body.arrayBuffer();
      }
      if (this._body instanceof Uint8Array) {
        return this._body.buffer;
      }
      if (this._body instanceof ArrayBuffer) {
        return this._body;
      }
      if (this._body instanceof URLSearchParams || this._body instanceof FormData) {
        return encodeUtf8(this._body.toString()).buffer;
      }
      const bytes = encodeUtf8(typeof this._body === "string" ? this._body : String(this._body));
      return bytes.buffer;
    }

    async blob() {
      this.bodyUsed = true;
      if (this._body instanceof Blob) {
        return this._body;
      }
      if (this._body instanceof Uint8Array) {
        return new Blob([this._body]);
      }
      if (this._body instanceof ArrayBuffer) {
        return new Blob([new Uint8Array(this._body)]);
      }
      if (this._body instanceof URLSearchParams || this._body instanceof FormData) {
        return new Blob([this._body.toString()]);
      }
      return new Blob([typeof this._body === "string" ? this._body : String(this._body)]);
    }

    async formData() {
      throw new Error("FormData not implemented");
    }
  }

  class Response extends BodyMixin {
    constructor(body, init = {}) {
      super(body);
      this.status = init.status ?? 200;
      this.statusText = init.statusText ?? "";
      this.url = init.url ?? "";
      this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
      this.ok = this.status >= 200 && this.status < 300;
      this.redirected = false;
      this.type = "basic";
    }

    clone() {
      return new Response(this._body, {
        status: this.status,
        statusText: this.statusText,
        headers: new Headers(this.headers),
        url: this.url,
      });
    }

    static json(value, init = {}) {
      const body = JSON.stringify(value);
      const headers = new Headers(init.headers);
      headers.set("content-type", "application/json");
      return new Response(body, {
        ...init,
        headers,
        status: init.status ?? 200,
        statusText: init.statusText ?? "",
      });
    }

    static redirect(url, status = 302) {
      const headers = new Headers();
      headers.set("location", url);
      const response = new Response("", { status, headers });
      response.redirected = true;
      response.type = "opaqueredirect";
      return response;
    }

    static error() {
      const response = new Response("", { status: 0, statusText: "" });
      response.type = "error";
      return response;
    }
  }

  class Request extends BodyMixin {
    constructor(input, init = {}) {
      if (input instanceof Request) {
        super(init.body ?? input._body);
        this.url = input.url;
        this.method = (init.method || input.method || "GET").toUpperCase();
        this.headers = new Headers(init.headers || input.headers);
        this.mode = init.mode ?? input.mode ?? "cors";
        this.credentials = init.credentials ?? input.credentials ?? "same-origin";
        this.cache = init.cache ?? input.cache ?? "default";
        this.redirect = init.redirect ?? input.redirect ?? "follow";
        this.referrer = init.referrer ?? input.referrer ?? "about:client";
        this.referrerPolicy = init.referrerPolicy ?? input.referrerPolicy ?? "";
        this.integrity = init.integrity ?? input.integrity ?? "";
        this.keepalive = init.keepalive ?? input.keepalive ?? false;
        this.signal = init.signal ?? input.signal ?? undefined;
        this.destination = input.destination ?? "";
      } else {
        super(init.body);
        this.url = String(input);
        this.method = (init.method || "GET").toUpperCase();
        this.headers = new Headers(init.headers);
        this.mode = init.mode ?? "cors";
        this.credentials = init.credentials ?? "same-origin";
        this.cache = init.cache ?? "default";
        this.redirect = init.redirect ?? "follow";
        this.referrer = init.referrer ?? "about:client";
        this.referrerPolicy = init.referrerPolicy ?? "";
        this.integrity = init.integrity ?? "";
        this.keepalive = init.keepalive ?? false;
        this.signal = init.signal ?? undefined;
        this.destination = "";
      }
    }

    clone() {
      return new Request(this, {
        body: this._body,
        method: this.method,
        headers: new Headers(this.headers),
        mode: this.mode,
        credentials: this.credentials,
        cache: this.cache,
        redirect: this.redirect,
        referrer: this.referrer,
        referrerPolicy: this.referrerPolicy,
        integrity: this.integrity,
        keepalive: this.keepalive,
        signal: this.signal,
      });
    }
  }

  async function fetch(input, init) {
    const request = new Request(input, init);
    let body = request._body;

    if (body instanceof Blob) {
      body = await body.text();
    } else if (body instanceof Uint8Array) {
      body = decodeUtf8(body);
    } else if (body instanceof ArrayBuffer) {
      body = decodeUtf8(new Uint8Array(body));
    } else if (body instanceof URLSearchParams) {
      if (!request.headers.has("content-type")) {
        request.headers.set("content-type", "application/x-www-form-urlencoded;charset=UTF-8");
      }
      body = body.toString();
    } else if (body instanceof FormData) {
      const boundary = `----otterformdata${Math.random().toString(16).slice(2)}`;
      if (!request.headers.has("content-type")) {
        request.headers.set("content-type", `multipart/form-data; boundary=${boundary}`);
      }
      let formBody = "";
      body.forEach((value, key) => {
        formBody += `--${boundary}\r\n`;
        formBody += `Content-Disposition: form-data; name=\"${key}\"\r\n\r\n`;
        formBody += `${value}\r\n`;
      });
      formBody += `--${boundary}--\r\n`;
      body = formBody;
    }

    const raw = await global.__otter_fetch_raw(request.url, {
      method: request.method,
      headers: Object.fromEntries(request.headers.entries()),
      body,
    });

    const headers = new Headers(raw.headers || {});
    return new Response(raw.bodyText ?? "", {
      status: raw.status,
      statusText: raw.statusText,
      headers,
      url: raw.url,
    });
  }

  // TextEncoder - Web standard API for encoding strings to UTF-8
  class TextEncoder {
    constructor() {
      this.encoding = "utf-8";
    }

    encode(input = "") {
      return encodeUtf8(String(input));
    }

    encodeInto(source, destination) {
      const encoded = encodeUtf8(String(source));
      const len = Math.min(encoded.length, destination.length);
      for (let i = 0; i < len; i++) {
        destination[i] = encoded[i];
      }
      return { read: source.length, written: len };
    }
  }

  // TextDecoder - Web standard API for decoding UTF-8 to strings
  class TextDecoder {
    constructor(label = "utf-8", options = {}) {
      const normalizedLabel = String(label).toLowerCase().trim();
      if (normalizedLabel !== "utf-8" && normalizedLabel !== "utf8") {
        throw new RangeError(`TextDecoder: '${label}' encoding is not supported`);
      }
      this.encoding = "utf-8";
      this.fatal = Boolean(options.fatal);
      this.ignoreBOM = Boolean(options.ignoreBOM);
    }

    decode(input, _options = {}) {
      if (input === undefined || input === null) {
        return "";
      }
      let bytes;
      if (input instanceof Uint8Array) {
        bytes = input;
      } else if (input instanceof ArrayBuffer) {
        bytes = new Uint8Array(input);
      } else if (ArrayBuffer.isView(input)) {
        bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
      } else {
        throw new TypeError("TextDecoder.decode: input must be a BufferSource");
      }
      return decodeUtf8(bytes);
    }
  }

  global.Headers = Headers;
  global.Request = Request;
  global.Response = Response;
  global.Blob = Blob;
  global.FormData = FormData;
  global.URLSearchParams = URLSearchParams;
  global.fetch = fetch;
  global.TextEncoder = TextEncoder;
  global.TextDecoder = TextDecoder;
})(globalThis);
