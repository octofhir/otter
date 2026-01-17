/**
 * Node.js querystring module implementation for Otter.
 *
 * Provides utilities for parsing and formatting URL query strings.
 */
(function (global) {
  "use strict";

  /**
   * URL-encode a string (percent-encoding).
   * Similar to encodeURIComponent but with some differences for query strings.
   *
   * @param {string} str - String to encode
   * @returns {string} Encoded string
   */
  function escape(str) {
    if (typeof str !== "string") {
      if (typeof str === "object") {
        str = String(str);
      } else {
        str = str + "";
      }
    }

    return encodeURIComponent(str)
      .replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
  }

  /**
   * URL-decode a string.
   *
   * @param {string} str - String to decode
   * @returns {string} Decoded string
   */
  function unescape(str) {
    if (typeof str !== "string") {
      str = String(str);
    }

    try {
      return decodeURIComponent(str.replace(/\+/g, " "));
    } catch {
      // Return original string if decoding fails
      return str;
    }
  }

  /**
   * Parse a query string into an object.
   *
   * @param {string} str - Query string to parse
   * @param {string} [sep='&'] - Separator between key-value pairs
   * @param {string} [eq='='] - Separator between keys and values
   * @param {Object} [options] - Parse options
   * @param {number} [options.maxKeys=1000] - Maximum number of keys to parse
   * @param {Function} [options.decodeURIComponent] - Custom decode function
   * @returns {Object} Parsed query object
   */
  function parse(str, sep, eq, options) {
    sep = sep || "&";
    eq = eq || "=";

    const obj = Object.create(null);

    if (typeof str !== "string" || str.length === 0) {
      return obj;
    }

    // Remove leading ? if present
    if (str.charCodeAt(0) === 0x3f) {
      str = str.slice(1);
    }

    const decode = (options && options.decodeURIComponent) || unescape;
    const maxKeys = (options && typeof options.maxKeys === "number") ? options.maxKeys : 1000;

    const pairs = str.split(sep);
    const len = maxKeys > 0 ? Math.min(pairs.length, maxKeys) : pairs.length;

    for (let i = 0; i < len; i++) {
      const pair = pairs[i];
      const eqIdx = pair.indexOf(eq);

      let key, value;

      if (eqIdx < 0) {
        key = decode(pair);
        value = "";
      } else {
        key = decode(pair.slice(0, eqIdx));
        value = decode(pair.slice(eqIdx + eq.length));
      }

      // Handle duplicate keys by converting to array
      if (Object.prototype.hasOwnProperty.call(obj, key)) {
        const existing = obj[key];
        if (Array.isArray(existing)) {
          existing.push(value);
        } else {
          obj[key] = [existing, value];
        }
      } else {
        obj[key] = value;
      }
    }

    return obj;
  }

  /**
   * Stringify an object into a query string.
   *
   * @param {Object} obj - Object to stringify
   * @param {string} [sep='&'] - Separator between key-value pairs
   * @param {string} [eq='='] - Separator between keys and values
   * @param {Object} [options] - Stringify options
   * @param {Function} [options.encodeURIComponent] - Custom encode function
   * @returns {string} Query string
   */
  function stringify(obj, sep, eq, options) {
    sep = sep || "&";
    eq = eq || "=";

    if (obj === null || obj === undefined || typeof obj !== "object") {
      return "";
    }

    const encode = (options && options.encodeURIComponent) || escape;
    const keys = Object.keys(obj);
    const pairs = [];

    for (const key of keys) {
      const value = obj[key];
      const encodedKey = encode(key);

      if (Array.isArray(value)) {
        for (const v of value) {
          pairs.push(encodedKey + eq + encode(stringifyPrimitive(v)));
        }
      } else {
        pairs.push(encodedKey + eq + encode(stringifyPrimitive(value)));
      }
    }

    return pairs.join(sep);
  }

  /**
   * Convert a primitive value to string for query string.
   *
   * @param {*} v - Value to stringify
   * @returns {string} String representation
   */
  function stringifyPrimitive(v) {
    if (typeof v === "string") {
      return v;
    }
    if (typeof v === "number" && isFinite(v)) {
      return String(v);
    }
    if (typeof v === "boolean") {
      return v ? "true" : "false";
    }
    if (typeof v === "bigint") {
      return String(v);
    }
    return "";
  }

  // Aliases
  const encode = stringify;
  const decode = parse;

  // ==========================================================================
  // Module exports
  // ==========================================================================

  const querystring = {
    parse,
    stringify,
    decode,
    encode,
    escape,
    unescape,
  };

  // Register as node:querystring module
  if (typeof __registerModule === "function") {
    __registerModule("querystring", querystring);
  }

  // Also expose on global for direct access
  global.__otter_querystring = querystring;
})(globalThis);
