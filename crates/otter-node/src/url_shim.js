/**
 * URL and URLSearchParams Web API implementation for Otter.
 *
 * WHATWG URL Standard compliant implementation using native Rust ops.
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

  // Export to global
  global.URL = URL;
  global.URLSearchParams = URLSearchParams;
})(globalThis);
