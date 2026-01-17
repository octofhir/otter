/**
 * URL and URLSearchParams Web API + Node.js url module implementation for Otter.
 *
 * WHATWG URL Standard compliant implementation using native Rust ops,
 * plus Node.js legacy url.parse/format/resolve APIs.
 */
(function (global) {
  "use strict";

  /**
   * URLSearchParams class for working with query strings.
   */
  class URLSearchParams {
    #params = [];

    constructor(init) {
      if (init === undefined || init === null) {
        return;
      }

      if (typeof init === "string") {
        // Parse query string
        const query = init.startsWith("?") ? init.slice(1) : init;
        if (query) {
          for (const pair of query.split("&")) {
            const [key, ...valueParts] = pair.split("=");
            const value = valueParts.join("=");
            this.#params.push([
              decodeURIComponent(key.replace(/\+/g, " ")),
              decodeURIComponent(value.replace(/\+/g, " ")),
            ]);
          }
        }
      } else if (Array.isArray(init)) {
        // Array of [key, value] pairs
        for (const [key, value] of init) {
          this.#params.push([String(key), String(value)]);
        }
      } else if (init instanceof URLSearchParams) {
        // Copy from another URLSearchParams
        for (const [key, value] of init) {
          this.#params.push([key, value]);
        }
      } else if (typeof init === "object") {
        // Object with key-value pairs
        for (const [key, value] of Object.entries(init)) {
          this.#params.push([key, String(value)]);
        }
      }
    }

    append(name, value) {
      this.#params.push([String(name), String(value)]);
    }

    delete(name) {
      this.#params = this.#params.filter(([key]) => key !== name);
    }

    get(name) {
      const pair = this.#params.find(([key]) => key === name);
      return pair ? pair[1] : null;
    }

    getAll(name) {
      return this.#params.filter(([key]) => key === name).map(([, value]) => value);
    }

    has(name) {
      return this.#params.some(([key]) => key === name);
    }

    set(name, value) {
      const strName = String(name);
      const strValue = String(value);
      let found = false;

      this.#params = this.#params.filter(([key]) => {
        if (key === strName) {
          if (!found) {
            found = true;
            return true;
          }
          return false;
        }
        return true;
      });

      if (found) {
        const idx = this.#params.findIndex(([key]) => key === strName);
        this.#params[idx][1] = strValue;
      } else {
        this.#params.push([strName, strValue]);
      }
    }

    sort() {
      this.#params.sort((a, b) => a[0].localeCompare(b[0]));
    }

    get size() {
      return this.#params.length;
    }

    toString() {
      return this.#params
        .map(
          ([key, value]) =>
            `${encodeURIComponent(key).replace(/%20/g, "+")}=${encodeURIComponent(value).replace(/%20/g, "+")}`
        )
        .join("&");
    }

    *entries() {
      for (const pair of this.#params) {
        yield pair;
      }
    }

    *keys() {
      for (const [key] of this.#params) {
        yield key;
      }
    }

    *values() {
      for (const [, value] of this.#params) {
        yield value;
      }
    }

    forEach(callback, thisArg) {
      for (const [key, value] of this.#params) {
        callback.call(thisArg, value, key, this);
      }
    }

    [Symbol.iterator]() {
      return this.entries();
    }
  }

  /**
   * URL class for parsing and manipulating URLs.
   */
  class URL {
    #components;
    #searchParams;

    constructor(url, base) {
      if (url === undefined) {
        throw new TypeError("Failed to construct 'URL': 1 argument required");
      }

      const urlStr = String(url);
      const baseStr = base !== undefined ? String(base) : null;

      // Parse URL using native op
      const result = __otter_url_parse(urlStr, baseStr);

      if (result.error) {
        throw new TypeError(`Failed to construct 'URL': ${result.error}`);
      }

      this.#components = result;
      this.#searchParams = new URLSearchParams(this.#components.search);

      // Keep searchParams in sync with URL
      this.#setupSearchParamsSync();
    }

    #setupSearchParamsSync() {
      // Create a proxy to sync changes back to URL
      const url = this;
      const originalSearchParams = this.#searchParams;

      // Override mutating methods to update URL
      const syncMethods = ["append", "delete", "set", "sort"];
      for (const method of syncMethods) {
        const original = originalSearchParams[method].bind(originalSearchParams);
        originalSearchParams[method] = function (...args) {
          const result = original(...args);
          url.#updateSearch();
          return result;
        };
      }
    }

    #updateSearch() {
      const newSearch = this.#searchParams.toString();
      this.#components.search = newSearch ? `?${newSearch}` : "";
      this.#updateHref();
    }

    #updateHref() {
      // Reconstruct href from components
      let href = this.#components.protocol + "//";

      if (this.#components.username) {
        href += this.#components.username;
        if (this.#components.password) {
          href += ":" + this.#components.password;
        }
        href += "@";
      }

      href += this.#components.host;
      href += this.#components.pathname;
      href += this.#components.search;
      href += this.#components.hash;

      this.#components.href = href;
    }

    get href() {
      return this.#components.href;
    }

    set href(value) {
      const result = __otter_url_parse(String(value), null);
      if (result.error) {
        throw new TypeError(`Invalid URL: ${result.error}`);
      }
      this.#components = result;
      this.#searchParams = new URLSearchParams(this.#components.search);
      this.#setupSearchParamsSync();
    }

    get origin() {
      return this.#components.origin;
    }

    get protocol() {
      return this.#components.protocol;
    }

    set protocol(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "protocol",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get username() {
      return this.#components.username;
    }

    set username(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "username",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get password() {
      return this.#components.password;
    }

    set password(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "password",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get host() {
      return this.#components.host;
    }

    set host(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "host",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get hostname() {
      return this.#components.hostname;
    }

    set hostname(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "hostname",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get port() {
      return this.#components.port;
    }

    set port(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "port",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get pathname() {
      return this.#components.pathname;
    }

    set pathname(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "pathname",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    get search() {
      return this.#components.search;
    }

    set search(value) {
      const strValue = String(value);
      const result = __otter_url_set_component(
        this.#components.href,
        "search",
        strValue
      );
      if (!result.error) {
        this.#components = result;
        this.#searchParams = new URLSearchParams(strValue);
        this.#setupSearchParamsSync();
      }
    }

    get searchParams() {
      return this.#searchParams;
    }

    get hash() {
      return this.#components.hash;
    }

    set hash(value) {
      const result = __otter_url_set_component(
        this.#components.href,
        "hash",
        String(value)
      );
      if (!result.error) {
        this.#components = result;
      }
    }

    toString() {
      return this.#components.href;
    }

    toJSON() {
      return this.#components.href;
    }

    /**
     * Static method to check if a URL can be parsed.
     */
    static canParse(url, base) {
      try {
        new URL(url, base);
        return true;
      } catch {
        return false;
      }
    }

    /**
     * Static method to parse a URL (returns null on error instead of throwing).
     */
    static parse(url, base) {
      try {
        return new URL(url, base);
      } catch {
        return null;
      }
    }
  }

  // ==========================================================================
  // Node.js Legacy URL API (url.parse, url.format, url.resolve)
  // ==========================================================================

  /**
   * Url class - the result of url.parse() (legacy API).
   * Matches Node.js url.Url structure.
   */
  class Url {
    constructor() {
      this.protocol = null;
      this.slashes = null;
      this.auth = null;
      this.host = null;
      this.port = null;
      this.hostname = null;
      this.hash = null;
      this.search = null;
      this.query = null;
      this.pathname = null;
      this.path = null;
      this.href = null;
    }
  }

  /**
   * Parse a URL string into an object (legacy Node.js API).
   *
   * @param {string} urlString - The URL string to parse
   * @param {boolean} [parseQueryString=false] - If true, parse query string into object
   * @param {boolean} [slashesDenoteHost=false] - If true, treat //foo as host
   * @returns {Url} Parsed URL object
   */
  function parse(urlString, parseQueryString = false, slashesDenoteHost = false) {
    const result = new Url();

    if (typeof urlString !== "string") {
      urlString = String(urlString);
    }

    // Trim whitespace
    urlString = urlString.trim();
    result.href = urlString;

    // Handle protocol-relative URLs
    let rest = urlString;
    let proto = null;

    // Extract protocol
    const protoMatch = rest.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):(.*)$/);
    if (protoMatch) {
      proto = protoMatch[1].toLowerCase();
      result.protocol = proto + ":";
      rest = protoMatch[2];
    }

    // Check for slashes
    let slashes = false;
    if (rest.startsWith("//")) {
      slashes = true;
      rest = rest.slice(2);
      result.slashes = true;
    } else if (slashesDenoteHost && !proto) {
      // For protocol-relative URLs like //example.com
      slashes = true;
      result.slashes = true;
    }

    // If we have slashes, parse host
    if (slashes || (proto && rest && !rest.startsWith("/"))) {
      // Extract auth@host:port
      let hostEnd = rest.length;

      // Find where host ends (at /, ?, or #)
      for (let i = 0; i < rest.length; i++) {
        const c = rest[i];
        if (c === "/" || c === "?" || c === "#") {
          hostEnd = i;
          break;
        }
      }

      let hostPart = rest.slice(0, hostEnd);
      rest = rest.slice(hostEnd);

      // Parse auth
      const atIndex = hostPart.lastIndexOf("@");
      if (atIndex !== -1) {
        result.auth = decodeURIComponent(hostPart.slice(0, atIndex));
        hostPart = hostPart.slice(atIndex + 1);
      }

      // Parse host and port
      result.host = hostPart;

      // Handle IPv6
      if (hostPart.startsWith("[")) {
        const bracketEnd = hostPart.indexOf("]");
        if (bracketEnd !== -1) {
          result.hostname = hostPart.slice(0, bracketEnd + 1);
          const afterBracket = hostPart.slice(bracketEnd + 1);
          if (afterBracket.startsWith(":")) {
            result.port = afterBracket.slice(1);
          }
        }
      } else {
        const colonIndex = hostPart.lastIndexOf(":");
        if (colonIndex !== -1) {
          result.hostname = hostPart.slice(0, colonIndex);
          result.port = hostPart.slice(colonIndex + 1);
        } else {
          result.hostname = hostPart;
        }
      }

      // Lowercase hostname
      if (result.hostname) {
        result.hostname = result.hostname.toLowerCase();
      }
    }

    // Parse hash
    const hashIndex = rest.indexOf("#");
    if (hashIndex !== -1) {
      result.hash = rest.slice(hashIndex);
      rest = rest.slice(0, hashIndex);
    }

    // Parse search/query
    const searchIndex = rest.indexOf("?");
    if (searchIndex !== -1) {
      result.search = rest.slice(searchIndex);
      rest = rest.slice(0, searchIndex);

      if (parseQueryString) {
        // Parse query string into object
        const queryStr = result.search.slice(1);
        const query = {};
        for (const pair of queryStr.split("&")) {
          if (!pair) continue;
          const eqIndex = pair.indexOf("=");
          let key, value;
          if (eqIndex === -1) {
            key = decodeURIComponent(pair);
            value = "";
          } else {
            key = decodeURIComponent(pair.slice(0, eqIndex));
            value = decodeURIComponent(pair.slice(eqIndex + 1));
          }
          if (Object.prototype.hasOwnProperty.call(query, key)) {
            if (Array.isArray(query[key])) {
              query[key].push(value);
            } else {
              query[key] = [query[key], value];
            }
          } else {
            query[key] = value;
          }
        }
        result.query = query;
      } else {
        result.query = result.search.slice(1);
      }
    }

    // Pathname
    result.pathname = rest || null;

    // Path = pathname + search
    if (result.pathname || result.search) {
      result.path = (result.pathname || "") + (result.search || "");
    }

    return result;
  }

  /**
   * Format a URL object into a string (legacy Node.js API).
   *
   * @param {Url|URL|Object} urlObject - URL object to format
   * @returns {string} Formatted URL string
   */
  function format(urlObject) {
    // Handle WHATWG URL
    if (urlObject instanceof URL) {
      return urlObject.href;
    }

    let result = "";

    // Protocol
    if (urlObject.protocol) {
      result += urlObject.protocol;
      if (!urlObject.protocol.endsWith(":")) {
        result += ":";
      }
    }

    // Slashes
    if (urlObject.slashes || urlObject.protocol === "http:" || urlObject.protocol === "https:") {
      result += "//";
    }

    // Auth
    if (urlObject.auth) {
      result += encodeURIComponent(urlObject.auth) + "@";
    }

    // Host/hostname
    if (urlObject.host) {
      result += urlObject.host;
    } else if (urlObject.hostname) {
      result += urlObject.hostname;
      if (urlObject.port) {
        result += ":" + urlObject.port;
      }
    }

    // Pathname
    if (urlObject.pathname) {
      result += urlObject.pathname;
    }

    // Search/query
    if (urlObject.search) {
      result += urlObject.search;
    } else if (urlObject.query) {
      if (typeof urlObject.query === "object") {
        const pairs = [];
        for (const [key, value] of Object.entries(urlObject.query)) {
          if (Array.isArray(value)) {
            for (const v of value) {
              pairs.push(encodeURIComponent(key) + "=" + encodeURIComponent(v));
            }
          } else if (value !== undefined && value !== null) {
            pairs.push(encodeURIComponent(key) + "=" + encodeURIComponent(value));
          }
        }
        if (pairs.length > 0) {
          result += "?" + pairs.join("&");
        }
      } else if (urlObject.query) {
        result += "?" + urlObject.query;
      }
    }

    // Hash
    if (urlObject.hash) {
      result += urlObject.hash;
    }

    return result;
  }

  /**
   * Resolve a target URL relative to a base URL (legacy Node.js API).
   *
   * @param {string} from - Base URL
   * @param {string} to - Target URL (relative or absolute)
   * @returns {string} Resolved URL
   */
  function resolve(from, to) {
    // Use WHATWG URL for resolution
    try {
      return new URL(to, from).href;
    } catch {
      // Fallback for invalid URLs
      return to;
    }
  }

  // ==========================================================================
  // File URL utilities
  // ==========================================================================

  /**
   * Convert a file:// URL to a filesystem path.
   *
   * @param {string|URL} url - file:// URL
   * @returns {string} Filesystem path
   */
  function fileURLToPath(url) {
    if (typeof url === "string") {
      url = new URL(url);
    }

    if (url.protocol !== "file:") {
      throw new TypeError("The URL must be of scheme file");
    }

    // Get pathname and decode
    let pathname = decodeURIComponent(url.pathname);

    // Handle Windows paths (e.g., file:///C:/path)
    const isWindows = typeof process !== "undefined" && process.platform === "win32";
    if (isWindows && pathname.match(/^\/[A-Za-z]:\//)) {
      pathname = pathname.slice(1); // Remove leading slash
      pathname = pathname.replace(/\//g, "\\"); // Convert to backslashes
    }

    return pathname;
  }

  /**
   * Convert a filesystem path to a file:// URL.
   *
   * @param {string} path - Filesystem path
   * @returns {URL} file:// URL
   */
  function pathToFileURL(path) {
    if (typeof path !== "string") {
      throw new TypeError("The path argument must be of type string");
    }

    // Handle Windows paths
    const isWindows = typeof process !== "undefined" && process.platform === "win32";

    let pathname = path;
    if (isWindows) {
      pathname = pathname.replace(/\\/g, "/");
      // Add leading slash for absolute paths
      if (pathname.match(/^[A-Za-z]:\//)) {
        pathname = "/" + pathname;
      }
    }

    // Make sure it starts with /
    if (!pathname.startsWith("/")) {
      pathname = "/" + pathname;
    }

    // Encode special characters (but not /)
    pathname = pathname
      .split("/")
      .map((segment) => encodeURIComponent(segment))
      .join("/");

    return new URL("file://" + pathname);
  }

  /**
   * Convert domain to ASCII (punycode).
   * Simple implementation - for full punycode, would need punycode library.
   *
   * @param {string} domain - Domain name
   * @returns {string} ASCII domain
   */
  function domainToASCII(domain) {
    try {
      const url = new URL("http://" + domain);
      return url.hostname;
    } catch {
      return "";
    }
  }

  /**
   * Convert domain from ASCII (punycode) to Unicode.
   * Simple implementation.
   *
   * @param {string} domain - ASCII domain
   * @returns {string} Unicode domain
   */
  function domainToUnicode(domain) {
    // For now, just return as-is (proper implementation would need punycode)
    return domain;
  }

  // ==========================================================================
  // Module exports
  // ==========================================================================

  const urlModule = {
    // WHATWG URL API
    URL,
    URLSearchParams,

    // Legacy API
    Url,
    parse,
    format,
    resolve,

    // File URL utilities
    fileURLToPath,
    pathToFileURL,

    // Domain utilities
    domainToASCII,
    domainToUnicode,
  };

  // Register as node:url module
  if (typeof __registerModule === "function") {
    __registerModule("url", urlModule);
  }

  // Export to global
  global.URL = URL;
  global.URLSearchParams = URLSearchParams;

  // Also expose module for direct access
  global.__otter_url = urlModule;
})(globalThis);
