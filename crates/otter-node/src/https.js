/**
 * node:https - Node.js compatible HTTPS module.
 *
 * Reuses classes from the http module with HTTPS-specific defaults.
 * Uses fetch() for client requests with https: protocol.
 */
(function() {
    'use strict';

    // Get the http module for shared classes
    const http = globalThis.__otter_get_node_builtin('http');

    if (!http) {
        console.warn('https module: http module not available');
    }

    /**
     * HTTPS-specific Agent - extends http.Agent with HTTPS defaults
     */
    class Agent extends (http?.Agent || class {}) {
        constructor(options = {}) {
            super({
                ...options,
                defaultPort: 443,
                protocol: 'https:',
            });

            // HTTPS-specific options
            this.defaultPort = 443;
            this.protocol = 'https:';

            // TLS options (for compatibility, not fully used with fetch)
            this.maxCachedSessions = options.maxCachedSessions || 100;
            this.rejectUnauthorized = options.rejectUnauthorized !== false;
            this.servername = options.servername;
            this.ca = options.ca;
            this.cert = options.cert;
            this.key = options.key;
            this.pfx = options.pfx;
            this.passphrase = options.passphrase;
            this.ciphers = options.ciphers;
            this.secureProtocol = options.secureProtocol;
            this.minVersion = options.minVersion;
            this.maxVersion = options.maxVersion;
        }

        getName(options) {
            // Include TLS session key in name for proper caching
            let name = super.getName ? super.getName(options) : `${options.host || options.hostname || 'localhost'}:${options.port || 443}:`;

            // Add servername to cache key
            if (options.servername) {
                name += `:${options.servername}`;
            }

            return name;
        }
    }

    // Global HTTPS agent
    const globalAgent = new Agent({ keepAlive: true });

    /**
     * Make an HTTPS request - delegates to http.ClientRequest with https protocol
     */
    function request(urlOrOptions, optionsOrCallback, callback) {
        let options = urlOrOptions;

        if (typeof urlOrOptions === 'string' || urlOrOptions instanceof URL) {
            const url = typeof urlOrOptions === 'string' ? new URL(urlOrOptions) : urlOrOptions;
            options = {
                protocol: 'https:',
                hostname: url.hostname,
                port: url.port || 443,
                path: url.pathname + url.search,
            };
            if (typeof optionsOrCallback === 'function') {
                callback = optionsOrCallback;
            } else if (optionsOrCallback && typeof optionsOrCallback === 'object') {
                options = { ...options, ...optionsOrCallback };
            }
        } else if (typeof optionsOrCallback === 'function') {
            callback = optionsOrCallback;
        }

        // Ensure https protocol
        options = { ...options };
        options.protocol = 'https:';

        // Use HTTPS agent by default
        if (options.agent === undefined) {
            options.agent = globalAgent;
        }

        // Use http.ClientRequest
        if (http?.ClientRequest) {
            return new http.ClientRequest(options, callback);
        }

        // Fallback if http module not available
        throw new Error('http module not available for https request');
    }

    /**
     * Make an HTTPS GET request
     */
    function get(urlOrOptions, optionsOrCallback, callback) {
        let options = urlOrOptions;

        if (typeof urlOrOptions === 'string' || urlOrOptions instanceof URL) {
            const url = typeof urlOrOptions === 'string' ? new URL(urlOrOptions) : urlOrOptions;
            options = {
                protocol: 'https:',
                hostname: url.hostname,
                port: url.port || 443,
                path: url.pathname + url.search,
                method: 'GET',
            };
            if (typeof optionsOrCallback === 'function') {
                callback = optionsOrCallback;
            } else if (optionsOrCallback && typeof optionsOrCallback === 'object') {
                options = { ...options, ...optionsOrCallback };
            }
        } else if (typeof optionsOrCallback === 'function') {
            callback = optionsOrCallback;
            options = { ...options, method: 'GET' };
        } else {
            options = { ...options, method: 'GET' };
        }

        options.protocol = 'https:';

        const req = request(options, callback);
        req.end();
        return req;
    }

    /**
     * Create an HTTPS server
     * Note: This requires TLS certificates to be provided in options
     */
    function createServer(options, requestListener) {
        if (typeof options === 'function') {
            requestListener = options;
            options = {};
        }

        // Mark as secure for TLS handling
        const serverOptions = {
            ...options,
            secure: true,
        };

        // Use http.createServer with secure option
        if (http?.createServer) {
            return http.createServer(serverOptions, requestListener);
        }

        throw new Error('http module not available for https server');
    }

    // HTTPS module exports
    const httpsModule = {
        // Classes
        Agent,
        Server: http?.Server,

        // Factory functions
        createServer,
        request,
        get,

        // Agents
        globalAgent,

        // Re-export from http for compatibility
        STATUS_CODES: http?.STATUS_CODES,
        METHODS: http?.METHODS,
    };

    // Add default export
    httpsModule.default = httpsModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('https', httpsModule);
    }
})();
