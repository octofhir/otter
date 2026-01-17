/**
 * node:http - Node.js compatible HTTP module.
 *
 * Provides http.Server, http.IncomingMessage, http.ServerResponse and factory functions.
 * Uses Otter.serve() under the hood for high-performance HTTP handling.
 */
(function() {
    'use strict';

    // Get EventEmitter from the runtime
    const EventEmitter = globalThis.__otter_node_builtins?.EventEmitter ||
        (globalThis.require && globalThis.require('events').EventEmitter) ||
        class EventEmitter {
            constructor() {
                this._events = {};
            }
            on(event, listener) {
                if (!this._events[event]) this._events[event] = [];
                this._events[event].push(listener);
                return this;
            }
            once(event, listener) {
                const wrapper = (...args) => {
                    this.off(event, wrapper);
                    listener.apply(this, args);
                };
                wrapper._original = listener;
                return this.on(event, wrapper);
            }
            off(event, listener) {
                if (!this._events[event]) return this;
                this._events[event] = this._events[event].filter(
                    l => l !== listener && l._original !== listener
                );
                return this;
            }
            emit(event, ...args) {
                if (!this._events[event]) return false;
                this._events[event].forEach(listener => listener.apply(this, args));
                return true;
            }
            removeListener(event, listener) { return this.off(event, listener); }
            addListener(event, listener) { return this.on(event, listener); }
            removeAllListeners(event) {
                if (event) {
                    delete this._events[event];
                } else {
                    this._events = {};
                }
                return this;
            }
        };

    // Symbol for internal data
    const kServer = Symbol('server');
    const kRequest = Symbol('request');
    const kResponse = Symbol('response');
    const kSocket = Symbol('socket');
    const kHeadersSent = Symbol('headersSent');

    /**
     * HTTP status codes (subset)
     */
    const STATUS_CODES = {
        100: 'Continue',
        101: 'Switching Protocols',
        200: 'OK',
        201: 'Created',
        204: 'No Content',
        301: 'Moved Permanently',
        302: 'Found',
        304: 'Not Modified',
        400: 'Bad Request',
        401: 'Unauthorized',
        403: 'Forbidden',
        404: 'Not Found',
        405: 'Method Not Allowed',
        500: 'Internal Server Error',
        501: 'Not Implemented',
        502: 'Bad Gateway',
        503: 'Service Unavailable',
    };

    /**
     * Parse headers object from native format to Node.js format.
     * Converts header names to lowercase.
     */
    function parseHeaders(rawHeaders) {
        const headers = {};
        if (rawHeaders && typeof rawHeaders === 'object') {
            for (const [key, value] of Object.entries(rawHeaders)) {
                headers[key.toLowerCase()] = value;
            }
        }
        return headers;
    }

    /**
     * Convert headers object to array format [key1, value1, key2, value2, ...]
     */
    function headersToRawHeaders(headers) {
        const raw = [];
        for (const [key, value] of Object.entries(headers)) {
            if (Array.isArray(value)) {
                for (const v of value) {
                    raw.push(key, v);
                }
            } else {
                raw.push(key, value);
            }
        }
        return raw;
    }

    /**
     * IncomingMessage represents an HTTP request received by a server.
     * @extends EventEmitter
     */
    class IncomingMessage extends EventEmitter {
        constructor(requestData) {
            super();

            this[kRequest] = requestData;
            this[kSocket] = null;

            // Request properties
            this.httpVersion = '1.1';
            this.httpVersionMajor = 1;
            this.httpVersionMinor = 1;
            this.complete = false;
            this.aborted = false;
            this.readable = true;

            // Parse request data
            this.method = requestData.method || 'GET';
            this.url = requestData.url || '/';
            this.headers = parseHeaders(requestData.headers);
            this.rawHeaders = headersToRawHeaders(requestData.headers || {});

            // Body handling
            this._body = requestData.body;
            this._bodyConsumed = false;
        }

        get connection() {
            return this[kSocket];
        }

        get socket() {
            return this[kSocket];
        }

        /**
         * Read the entire body as a string.
         * For compatibility with frameworks that consume body manually.
         */
        async text() {
            if (this._bodyConsumed) return '';
            this._bodyConsumed = true;
            if (typeof this._body === 'string') {
                return this._body;
            }
            if (this._body instanceof Uint8Array) {
                return new TextDecoder().decode(this._body);
            }
            return '';
        }

        /**
         * Read the entire body as JSON.
         */
        async json() {
            const text = await this.text();
            return JSON.parse(text);
        }

        /**
         * Set socket (called internally)
         */
        _setSocket(socket) {
            this[kSocket] = socket;
        }

        /**
         * Emit data and end events for the body.
         * Called by the server after creating the message.
         */
        _emitBody() {
            if (this._body && !this._bodyConsumed) {
                this._bodyConsumed = true;
                if (typeof this._body === 'string') {
                    this.emit('data', Buffer.from(this._body));
                } else if (this._body instanceof Uint8Array) {
                    this.emit('data', this._body);
                }
            }
            this.complete = true;
            this.readable = false;
            this.emit('end');
        }

        setTimeout(msecs, callback) {
            if (callback) this.once('timeout', callback);
            return this;
        }

        destroy(error) {
            this.aborted = true;
            if (error) this.emit('error', error);
            this.emit('close');
        }
    }

    /**
     * ServerResponse represents an HTTP response being sent back to the client.
     * @extends EventEmitter
     */
    class ServerResponse extends EventEmitter {
        constructor(req) {
            super();

            this[kRequest] = req;
            this[kSocket] = req[kSocket];
            this[kHeadersSent] = false;

            // Response properties
            this.statusCode = 200;
            this.statusMessage = '';
            this._headers = {};
            this._body = [];
            this._finished = false;
            this.writableEnded = false;
            this.writableFinished = false;

            // For sendResponse callback
            this._resolve = null;
            this._responsePromise = new Promise((resolve) => {
                this._resolve = resolve;
            });
        }

        get headersSent() {
            return this[kHeadersSent];
        }

        get connection() {
            return this[kSocket];
        }

        get socket() {
            return this[kSocket];
        }

        get finished() {
            return this._finished;
        }

        /**
         * Set a single header value.
         */
        setHeader(name, value) {
            if (this[kHeadersSent]) {
                throw new Error('Cannot set headers after they are sent');
            }
            this._headers[name.toLowerCase()] = value;
            return this;
        }

        /**
         * Get a header value.
         */
        getHeader(name) {
            return this._headers[name.toLowerCase()];
        }

        /**
         * Remove a header.
         */
        removeHeader(name) {
            if (this[kHeadersSent]) {
                throw new Error('Cannot remove headers after they are sent');
            }
            delete this._headers[name.toLowerCase()];
        }

        /**
         * Check if a header exists.
         */
        hasHeader(name) {
            return name.toLowerCase() in this._headers;
        }

        /**
         * Get all header names.
         */
        getHeaderNames() {
            return Object.keys(this._headers);
        }

        /**
         * Get all headers as an object.
         */
        getHeaders() {
            return { ...this._headers };
        }

        /**
         * Write status line and headers.
         */
        writeHead(statusCode, statusMessage, headers) {
            if (this[kHeadersSent]) {
                throw new Error('Cannot write headers after they are sent');
            }

            this.statusCode = statusCode;

            if (typeof statusMessage === 'string') {
                this.statusMessage = statusMessage;
            } else if (typeof statusMessage === 'object') {
                headers = statusMessage;
            }

            if (headers) {
                for (const [key, value] of Object.entries(headers)) {
                    this._headers[key.toLowerCase()] = value;
                }
            }

            this[kHeadersSent] = true;
            return this;
        }

        /**
         * Write data to the response body.
         */
        write(chunk, encoding, callback) {
            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = 'utf8';
            }

            if (this._finished) {
                const err = new Error('write after end');
                if (callback) callback(err);
                return false;
            }

            if (!this[kHeadersSent]) {
                this[kHeadersSent] = true;
            }

            if (typeof chunk === 'string') {
                this._body.push(chunk);
            } else if (chunk instanceof Uint8Array) {
                this._body.push(new TextDecoder().decode(chunk));
            } else if (chunk) {
                this._body.push(String(chunk));
            }

            if (callback) callback();
            return true;
        }

        /**
         * End the response, optionally writing final data.
         */
        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (this._finished) {
                if (callback) callback();
                return this;
            }

            if (data) {
                this.write(data, encoding);
            }

            this._finished = true;
            this.writableEnded = true;

            // Resolve the response promise with the final response
            const body = this._body.join('');
            const statusMessage = this.statusMessage || STATUS_CODES[this.statusCode] || '';

            this._resolve({
                status: this.statusCode,
                statusText: statusMessage,
                headers: this._headers,
                body: body,
            });

            this.writableFinished = true;
            this.emit('finish');
            this.emit('close');

            if (callback) callback();
            return this;
        }

        /**
         * Append a header value (for Set-Cookie etc.)
         */
        appendHeader(name, value) {
            const key = name.toLowerCase();
            const existing = this._headers[key];
            if (existing === undefined) {
                this._headers[key] = value;
            } else if (Array.isArray(existing)) {
                existing.push(value);
            } else {
                this._headers[key] = [existing, value];
            }
            return this;
        }

        /**
         * Flush headers (no-op, headers are sent on first write/end).
         */
        flushHeaders() {
            if (!this[kHeadersSent]) {
                this[kHeadersSent] = true;
            }
        }

        /**
         * Write continue response (100).
         */
        writeContinue() {
            // Not implemented in Otter.serve
        }

        setTimeout(msecs, callback) {
            if (callback) this.once('timeout', callback);
            return this;
        }
    }

    /**
     * HTTP Server class.
     * @extends EventEmitter
     */
    class Server extends EventEmitter {
        constructor(options, requestListener) {
            super();

            if (typeof options === 'function') {
                requestListener = options;
                options = {};
            }

            options = options || {};

            this[kServer] = null;
            this._listening = false;
            this._address = null;
            this._requestListener = requestListener;
            this._connections = 0;

            // Server options
            this.timeout = options.timeout || 0;
            this.keepAliveTimeout = options.keepAliveTimeout || 5000;
            this.maxHeadersCount = options.maxHeadersCount || 2000;
            this.headersTimeout = options.headersTimeout || 60000;
            this.requestTimeout = options.requestTimeout || 0;

            if (requestListener) {
                this.on('request', requestListener);
            }
        }

        /**
         * Start listening for connections.
         */
        listen(port, host, backlog, callback) {
            // Normalize arguments
            if (typeof port === 'object' && port !== null) {
                // listen({ port, host })
                const options = port;
                callback = host;
                port = options.port;
                host = options.host || '0.0.0.0';
            } else if (typeof host === 'function') {
                callback = host;
                host = '0.0.0.0';
            } else if (typeof backlog === 'function') {
                callback = backlog;
            }

            port = port || 0;
            host = host || '0.0.0.0';

            if (callback) {
                this.once('listening', callback);
            }

            // Use Otter.serve() under the hood
            const self = this;
            const serverPromise = Otter.serve({
                port,
                hostname: host,
                fetch: async (request) => {
                    return self._handleRequest(request);
                },
            });

            serverPromise.then((server) => {
                self[kServer] = server;
                self._listening = true;
                self._address = {
                    address: server.hostname || host,
                    port: server.port || port,
                    family: 'IPv4',
                };
                self.emit('listening');
            }).catch((err) => {
                self.emit('error', err);
            });

            return this;
        }

        /**
         * Handle incoming request from Otter.serve().
         */
        async _handleRequest(fetchRequest) {
            // Parse URL
            const url = new URL(fetchRequest.url);

            // Read body
            let body = null;
            try {
                body = await fetchRequest.text();
            } catch (e) {
                // Body might not be available
            }

            // Create request data
            const requestData = {
                method: fetchRequest.method,
                url: url.pathname + url.search,
                headers: Object.fromEntries(fetchRequest.headers.entries()),
                body: body,
            };

            // Create IncomingMessage and ServerResponse
            const req = new IncomingMessage(requestData);
            const res = new ServerResponse(req);

            // Emit request event
            this._connections++;
            this.emit('request', req, res);

            // Emit body events
            req._emitBody();

            // Wait for response
            const responseData = await res._responsePromise;
            this._connections--;

            // Convert headers for fetch Response
            const headers = new Headers();
            for (const [key, value] of Object.entries(responseData.headers)) {
                if (Array.isArray(value)) {
                    for (const v of value) {
                        headers.append(key, v);
                    }
                } else if (value !== undefined && value !== null) {
                    headers.set(key, String(value));
                }
            }

            // Return fetch Response
            return new Response(responseData.body, {
                status: responseData.status,
                statusText: responseData.statusText,
                headers,
            });
        }

        /**
         * Close the server.
         */
        close(callback) {
            if (callback) {
                this.once('close', callback);
            }

            if (this[kServer] && this[kServer].shutdown) {
                this[kServer].shutdown();
            }

            this._listening = false;
            this.emit('close');
            return this;
        }

        /**
         * Get the server's address.
         */
        address() {
            return this._address;
        }

        /**
         * Get connection count.
         */
        getConnections(callback) {
            callback(null, this._connections);
        }

        get listening() {
            return this._listening;
        }

        setTimeout(msecs, callback) {
            this.timeout = msecs;
            if (callback) this.on('timeout', callback);
            return this;
        }
    }

    /**
     * Create an HTTP server.
     */
    function createServer(options, requestListener) {
        return new Server(options, requestListener);
    }

    /**
     * HTTP methods
     */
    const METHODS = [
        'ACL', 'BIND', 'CHECKOUT', 'CONNECT', 'COPY', 'DELETE', 'GET', 'HEAD',
        'LINK', 'LOCK', 'M-SEARCH', 'MERGE', 'MKACTIVITY', 'MKCALENDAR',
        'MKCOL', 'MOVE', 'NOTIFY', 'OPTIONS', 'PATCH', 'POST', 'PRI',
        'PROPFIND', 'PROPPATCH', 'PURGE', 'PUT', 'REBIND', 'REPORT', 'SEARCH',
        'SOURCE', 'SUBSCRIBE', 'TRACE', 'UNBIND', 'UNLINK', 'UNLOCK',
        'UNSUBSCRIBE',
    ];

    // HTTP module
    const httpModule = {
        Server,
        IncomingMessage,
        ServerResponse,
        createServer,
        STATUS_CODES,
        METHODS,
        // maxHeaderSize - not implemented
        globalAgent: null, // Not implementing http.request/http.get yet
    };

    // Add default export
    httpModule.default = httpModule;

    // Register module
    if (globalThis.__registerModule) {
        globalThis.__registerModule('http', httpModule);
        globalThis.__registerModule('node:http', httpModule);
    }

    // Also expose for direct access
    if (globalThis.__otter_node_builtins) {
        globalThis.__otter_node_builtins.http = httpModule;
    }
})();
