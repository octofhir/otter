/**
 * Node.js dns module implementation for Otter.
 *
 * Provides DNS resolution functionality compatible with Node.js.
 */
(function (global) {
  "use strict";

  // ==========================================================================
  // Promise-based API (dns/promises or dns.promises)
  // ==========================================================================

  const promises = {
    /**
     * Resolve a hostname to an IP address.
     * @param {string} hostname - The hostname to resolve.
     * @param {object} [options] - Options.
     * @param {number} [options.family] - 4 or 6 for IPv4 or IPv6.
     * @returns {Promise<{address: string, family: number}>}
     */
    async lookup(hostname, options) {
      const family = typeof options === "number" ? options : options?.family;
      return __otter_dns_lookup(hostname, family);
    },

    /**
     * Resolve records of a specific type.
     * @param {string} hostname - The hostname to resolve.
     * @param {string} [rrtype='A'] - Record type.
     * @returns {Promise<any>}
     */
    async resolve(hostname, rrtype = "A") {
      return __otter_dns_resolve(hostname, rrtype);
    },

    /**
     * Resolve IPv4 addresses (A records).
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[]>}
     */
    async resolve4(hostname) {
      return __otter_dns_resolve4(hostname);
    },

    /**
     * Resolve IPv6 addresses (AAAA records).
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[]>}
     */
    async resolve6(hostname) {
      return __otter_dns_resolve6(hostname);
    },

    /**
     * Resolve MX records.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<Array<{exchange: string, priority: number}>>}
     */
    async resolveMx(hostname) {
      return __otter_dns_resolve_mx(hostname);
    },

    /**
     * Resolve TXT records.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[][]>}
     */
    async resolveTxt(hostname) {
      return __otter_dns_resolve_txt(hostname);
    },

    /**
     * Resolve NS records.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[]>}
     */
    async resolveNs(hostname) {
      return __otter_dns_resolve_ns(hostname);
    },

    /**
     * Resolve CNAME records.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[]>}
     */
    async resolveCname(hostname) {
      return __otter_dns_resolve_cname(hostname);
    },

    /**
     * Resolve SRV records.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<Array<{name: string, port: number, priority: number, weight: number}>>}
     */
    async resolveSrv(hostname) {
      return __otter_dns_resolve_srv(hostname);
    },

    /**
     * Resolve SOA record.
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<object>}
     */
    async resolveSoa(hostname) {
      return __otter_dns_resolve_soa(hostname);
    },

    /**
     * Resolve PTR records (reverse lookup).
     * @param {string} hostname - The hostname to resolve.
     * @returns {Promise<string[]>}
     */
    async resolvePtr(hostname) {
      return __otter_dns_resolve(hostname, "PTR");
    },

    /**
     * Reverse DNS lookup.
     * @param {string} ip - The IP address to look up.
     * @returns {Promise<string[]>}
     */
    async reverse(ip) {
      return __otter_dns_reverse(ip);
    },

    /**
     * Set custom DNS servers (not implemented).
     */
    setServers(servers) {
      // Store for getServers but not actually used
      promises._servers = servers;
    },

    /**
     * Get DNS servers.
     * @returns {string[]}
     */
    getServers() {
      return promises._servers || [];
    },
  };

  // ==========================================================================
  // Callback-based API (dns.lookup, dns.resolve, etc.)
  // ==========================================================================

  /**
   * Convert async function to callback style.
   */
  function callbackify(asyncFn) {
    return function (...args) {
      const callback = args.pop();
      if (typeof callback !== "function") {
        throw new TypeError("Callback must be a function");
      }

      asyncFn(...args)
        .then((result) => {
          if (asyncFn === promises.lookup) {
            // lookup returns {address, family} but callback expects (err, address, family)
            callback(null, result.address, result.family);
          } else {
            callback(null, result);
          }
        })
        .catch((err) => {
          const error = new Error(err.message || String(err));
          error.code = "ENOTFOUND";
          callback(error);
        });
    };
  }

  /**
   * Lookup a hostname.
   */
  function lookup(hostname, options, callback) {
    if (typeof options === "function") {
      callback = options;
      options = {};
    }

    if (typeof callback !== "function") {
      throw new TypeError("Callback must be a function");
    }

    promises
      .lookup(hostname, options)
      .then((result) => {
        callback(null, result.address, result.family);
      })
      .catch((err) => {
        const error = new Error(err.message || String(err));
        error.code = "ENOTFOUND";
        callback(error);
      });
  }

  // ==========================================================================
  // Error codes
  // ==========================================================================

  const NODATA = "ENODATA";
  const FORMERR = "EFORMERR";
  const SERVFAIL = "ESERVFAIL";
  const NOTFOUND = "ENOTFOUND";
  const NOTIMP = "ENOTIMP";
  const REFUSED = "EREFUSED";
  const BADQUERY = "EBADQUERY";
  const BADNAME = "EBADNAME";
  const BADFAMILY = "EBADFAMILY";
  const BADRESP = "EBADRESP";
  const CONNREFUSED = "ECONNREFUSED";
  const TIMEOUT = "ETIMEOUT";
  const EOF = "EOF";
  const FILE = "EFILE";
  const NOMEM = "ENOMEM";
  const DESTRUCTION = "EDESTRUCTION";
  const BADSTR = "EBADSTR";
  const BADFLAGS = "EBADFLAGS";
  const NONAME = "ENONAME";
  const BADHINTS = "EBADHINTS";
  const NOTINITIALIZED = "ENOTINITIALIZED";
  const LOADIPHLPAPI = "ELOADIPHLPAPI";
  const ADDRGETNETWORKPARAMS = "EADDRGETNETWORKPARAMS";
  const CANCELLED = "ECANCELLED";

  // ==========================================================================
  // Module exports
  // ==========================================================================

  const dns = {
    // Promise-based API
    promises,

    // Callback-based API
    lookup,
    resolve: callbackify(promises.resolve),
    resolve4: callbackify(promises.resolve4),
    resolve6: callbackify(promises.resolve6),
    resolveMx: callbackify(promises.resolveMx),
    resolveTxt: callbackify(promises.resolveTxt),
    resolveNs: callbackify(promises.resolveNs),
    resolveCname: callbackify(promises.resolveCname),
    resolveSrv: callbackify(promises.resolveSrv),
    resolveSoa: callbackify(promises.resolveSoa),
    resolvePtr: callbackify(promises.resolvePtr),
    reverse: callbackify(promises.reverse),

    // Server configuration
    setServers: promises.setServers,
    getServers: promises.getServers,

    // Error codes
    NODATA,
    FORMERR,
    SERVFAIL,
    NOTFOUND,
    NOTIMP,
    REFUSED,
    BADQUERY,
    BADNAME,
    BADFAMILY,
    BADRESP,
    CONNREFUSED,
    TIMEOUT,
    EOF,
    FILE,
    NOMEM,
    DESTRUCTION,
    BADSTR,
    BADFLAGS,
    NONAME,
    BADHINTS,
    NOTINITIALIZED,
    LOADIPHLPAPI,
    ADDRGETNETWORKPARAMS,
    CANCELLED,
  };

  // Register as node:dns module
  if (typeof __registerNodeBuiltin === "function") {
    __registerNodeBuiltin("dns", dns);
    __registerNodeBuiltin("dns/promises", promises);
  }

  // Also expose on global for direct access
  global.__otter_dns = dns;
})(globalThis);
