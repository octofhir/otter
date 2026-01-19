/**
 * node:http - Node.js compatible HTTP module.
 *
 * Provides complete Node.js http API:
 * - http.Server, http.IncomingMessage, http.ServerResponse
 * - http.ClientRequest, http.Agent, http.OutgoingMessage
 * - http.request(), http.get(), http.createServer()
 *
 * Uses Otter.serve() for server and fetch() for client requests.
 */
(function() {
    'use strict';

    // Get EventEmitter from the runtime
    const { EventEmitter } = globalThis.__otter_get_node_builtin('events');

    // Symbols for internal data
    const kServer = Symbol('server');
    const kRequest = Symbol('request');
    const kSocket = Symbol('socket');
    const kHeadersSent = Symbol('headersSent');

    /**
     * HTTP status codes (complete list)
     */
    const STATUS_CODES = {
        100: 'Continue',
        101: 'Switching Protocols',
        102: 'Processing',
        103: 'Early Hints',
        200: 'OK',
        201: 'Created',
        202: 'Accepted',
        203: 'Non-Authoritative Information',
        204: 'No Content',
        205: 'Reset Content',
        206: 'Partial Content',
        207: 'Multi-Status',
        208: 'Already Reported',
        226: 'IM Used',
        300: 'Multiple Choices',
        301: 'Moved Permanently',
        302: 'Found',
        303: 'See Other',
        304: 'Not Modified',
        305: 'Use Proxy',
        307: 'Temporary Redirect',
        308: 'Permanent Redirect',
        400: 'Bad Request',
        401: 'Unauthorized',
        402: 'Payment Required',
        403: 'Forbidden',
        404: 'Not Found',
        405: 'Method Not Allowed',
        406: 'Not Acceptable',
        407: 'Proxy Authentication Required',
        408: 'Request Timeout',
        409: 'Conflict',
        410: 'Gone',
        411: 'Length Required',
        412: 'Precondition Failed',
        413: 'Payload Too Large',
        414: 'URI Too Long',
        415: 'Unsupported Media Type',
        416: 'Range Not Satisfiable',
        417: 'Expectation Failed',
        418: "I'm a Teapot",
        421: 'Misdirected Request',
        422: 'Unprocessable Entity',
        423: 'Locked',
        424: 'Failed Dependency',
        425: 'Too Early',
        426: 'Upgrade Required',
        428: 'Precondition Required',
        429: 'Too Many Requests',
        431: 'Request Header Fields Too Large',
        451: 'Unavailable For Legal Reasons',
        500: 'Internal Server Error',
        501: 'Not Implemented',
        502: 'Bad Gateway',
        503: 'Service Unavailable',
        504: 'Gateway Timeout',
        505: 'HTTP Version Not Supported',
        506: 'Variant Also Negotiates',
        507: 'Insufficient Storage',
        508: 'Loop Detected',
        509: 'Bandwidth Limit Exceeded',
        510: 'Not Extended',
        511: 'Network Authentication Required',
    };

    /**
     * HTTP methods (complete list)
     */
    const METHODS = [
        'ACL', 'BIND', 'CHECKOUT', 'CONNECT', 'COPY', 'DELETE', 'GET', 'HEAD',
        'LINK', 'LOCK', 'M-SEARCH', 'MERGE', 'MKACTIVITY', 'MKCALENDAR',
        'MKCOL', 'MOVE', 'NOTIFY', 'OPTIONS', 'PATCH', 'POST', 'PRI',
        'PROPFIND', 'PROPPATCH', 'PURGE', 'PUT', 'REBIND', 'REPORT', 'SEARCH',
        'SOURCE', 'SUBSCRIBE', 'TRACE', 'UNBIND', 'UNLINK', 'UNLOCK',
        'UNSUBSCRIBE', 'UPDATE', 'UPDATEREDIRECTREF', 'VERSION-CONTROL',
    ];

    /**
     * Maximum header size (16KB default)
     */
    const maxHeaderSize = 16384;

    /**
     * Parse headers object from native format to Node.js format.
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
     * Validate header name
     */
    function validateHeaderName(name, label) {
        if (typeof name !== 'string' || name.length === 0) {
            throw new TypeError(`Header name must be a non-empty string`);
        }
        // RFC 7230 token
        if (!/^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$/.test(name)) {
            throw new TypeError(`Invalid header name: "${name}"`);
        }
    }

    /**
     * Validate header value
     */
    function validateHeaderValue(name, value) {
        if (value === undefined) {
            throw new TypeError(`Header "${name}" value must be defined`);
        }
        // Check for invalid characters (CR, LF)
        if (typeof value === 'string' && /[\r\n]/.test(value)) {
            throw new TypeError(`Invalid header value for "${name}"`);
        }
    }

    /**
     * Set max idle HTTP parsers
     */
    let _maxIdleHTTPParsers = 1000;
    function setMaxIdleHTTPParsers(max) {
        _maxIdleHTTPParsers = max;
    }

    // ============================================
    // OutgoingMessage - Base class for responses/requests
    // ============================================
    class OutgoingMessage extends EventEmitter {
        constructor() {
            super();
            this._headers = {};
            this._headersSent = false;
            this._finished = false;
            this._destroyed = false;
            this._socket = null;
            this._corked = 0;
            this._chunks = [];
            this._trailers = null;
        }

        get headersSent() { return this._headersSent; }
        get socket() { return this._socket; }
        get connection() { return this._socket; } // deprecated alias
        get writableEnded() { return this._finished; }
        get writableFinished() { return this._finished && this._chunks.length === 0; }
        get writableCorked() { return this._corked; }
        get writableHighWaterMark() { return 16384; }
        get writableLength() {
            return this._chunks.reduce((sum, c) => sum + (c ? c.length : 0), 0);
        }
        get writableObjectMode() { return false; }

        setHeader(name, value) {
            validateHeaderName(name);
            if (this._headersSent) {
                throw new Error('Cannot set headers after they are sent to the client');
            }
            this._headers[name.toLowerCase()] = value;
            return this;
        }

        getHeader(name) {
            return this._headers[name.toLowerCase()];
        }

        getHeaders() {
            return { ...this._headers };
        }

        getHeaderNames() {
            return Object.keys(this._headers);
        }

        hasHeader(name) {
            return name.toLowerCase() in this._headers;
        }

        removeHeader(name) {
            if (this._headersSent) {
                throw new Error('Cannot remove headers after they are sent to the client');
            }
            delete this._headers[name.toLowerCase()];
        }

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

        setHeaders(headers) {
            if (headers instanceof Map) {
                headers.forEach((value, key) => this.setHeader(key, value));
            } else if (headers && typeof headers === 'object') {
                for (const [key, value] of Object.entries(headers)) {
                    this.setHeader(key, value);
                }
            }
            return this;
        }

        flushHeaders() {
            this._headersSent = true;
        }

        addTrailers(headers) {
            this._trailers = headers;
        }

        cork() {
            this._corked++;
        }

        uncork() {
            if (this._corked > 0) {
                this._corked--;
                if (this._corked === 0) {
                    this._flush();
                }
            }
        }

        _flush() {
            // Override in subclasses to flush buffered chunks
        }

        write(chunk, encoding, callback) {
            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = 'utf8';
            }

            if (this._finished || this._destroyed) {
                const err = new Error('write after end');
                if (callback) callback(err);
                this.emit('error', err);
                return false;
            }

            if (chunk !== null && chunk !== undefined) {
                if (typeof chunk === 'string') {
                    this._chunks.push(chunk);
                } else if (chunk instanceof Uint8Array) {
                    this._chunks.push(new TextDecoder().decode(chunk));
                } else {
                    this._chunks.push(String(chunk));
                }
            }

            if (callback) callback();
            this.emit('drain');
            return true;
        }

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

            if (data !== null && data !== undefined) {
                this.write(data, encoding);
            }

            this._finished = true;
            this.emit('prefinish');
            this.emit('finish');

            if (callback) callback();
            return this;
        }

        destroy(error) {
            if (this._destroyed) return this;
            this._destroyed = true;
            if (error) this.emit('error', error);
            this.emit('close');
            return this;
        }

        setTimeout(msecs, callback) {
            if (callback) this.once('timeout', callback);
            return this;
        }

        pipe() {
            throw new Error('Cannot pipe from OutgoingMessage');
        }
    }

    // ============================================
    // Agent - Connection management
    // ============================================
    class Agent extends EventEmitter {
        constructor(options = {}) {
            super();
            this.options = options;
            this.keepAlive = options.keepAlive !== false;
            this.keepAliveMsecs = options.keepAliveMsecs || 1000;
            this.maxSockets = options.maxSockets || Infinity;
            this.maxTotalSockets = options.maxTotalSockets || Infinity;
            this.maxFreeSockets = options.maxFreeSockets || 256;
            this.scheduling = options.scheduling || 'lifo';
            this.timeout = options.timeout;
            this.defaultPort = options.defaultPort || 80;
            this.protocol = options.protocol || 'http:';

            this.sockets = {};      // Active sockets by name
            this.freeSockets = {};  // Free sockets by name
            this.requests = {};     // Pending requests by name
        }

        createConnection(options, callback) {
            // For compatibility - fetch handles connections internally
            const socket = {};
            if (callback) {
                queueMicrotask(() => callback(null, socket));
            }
            return socket;
        }

        getName(options) {
            const host = options.host || options.hostname || 'localhost';
            const port = options.port || this.defaultPort;
            const localAddress = options.localAddress || '';
            return `${host}:${port}:${localAddress}`;
        }

        keepSocketAlive(socket) {
            return true;
        }

        reuseSocket(socket, request) {
            // No-op: fetch handles socket reuse internally
        }

        destroy() {
            // Close all sockets
            this.sockets = {};
            this.freeSockets = {};
            this.requests = {};
        }
    }

    // Global agent instance
    const globalAgent = new Agent({ keepAlive: true });

    // ============================================
    // ClientRequest - Outgoing HTTP request
    // ============================================
    class ClientRequest extends OutgoingMessage {
        constructor(options, callback) {
            super();

            // Normalize options from URL
            if (typeof options === 'string') {
                options = new URL(options);
            }
            if (options instanceof URL) {
                options = {
                    protocol: options.protocol,
                    hostname: options.hostname,
                    port: options.port || undefined,
                    path: options.pathname + options.search,
                    hash: options.hash,
                };
            }

            this._options = options || {};
            this.method = (options.method || 'GET').toUpperCase();
            this.path = options.path || '/';
            this.host = options.hostname || options.host || 'localhost';
            this.protocol = options.protocol || 'http:';
            this._port = options.port || (this.protocol === 'https:' ? 443 : 80);
            this.agent = options.agent === undefined ? globalAgent :
                         options.agent === false ? new Agent({ maxSockets: Infinity }) :
                         options.agent;

            this.reusedSocket = false;
            this.maxHeadersCount = options.maxHeadersCount || 2000;
            this._aborted = false;
            this._ended = false;
            this._response = null;

            // Copy headers from options
            if (options.headers) {
                for (const [key, value] of Object.entries(options.headers)) {
                    this._headers[key.toLowerCase()] = value;
                }
            }

            // Set Host header if not present
            if (!this.hasHeader('host')) {
                let hostHeader = this.host;
                const defaultPort = this.protocol === 'https:' ? 443 : 80;
                if (this._port !== defaultPort) {
                    hostHeader += ':' + this._port;
                }
                this._headers['host'] = hostHeader;
            }

            // Build URL
            let portStr = '';
            const defaultPort = this.protocol === 'https:' ? 443 : 80;
            if (this._port !== defaultPort) {
                portStr = ':' + this._port;
            }
            this._url = `${this.protocol}//${this.host}${portStr}${this.path}`;

            // Auth support
            if (options.auth) {
                const encoded = typeof btoa === 'function'
                    ? btoa(options.auth)
                    : Buffer.from(options.auth).toString('base64');
                this._headers['authorization'] = `Basic ${encoded}`;
            }

            if (callback) {
                this.once('response', callback);
            }
        }

        get aborted() { return this._aborted; } // deprecated
        get destroyed() { return this._destroyed; }
        get finished() { return this._ended; } // deprecated, use writableEnded

        abort() {
            // Deprecated - use destroy()
            if (this._aborted) return;
            this._aborted = true;
            this._destroyed = true;
            this.emit('abort');
            this.emit('close');
        }

        destroy(error) {
            if (this._destroyed) return this;
            this._destroyed = true;
            this._aborted = true;
            if (error) this.emit('error', error);
            this.emit('close');
            return this;
        }

        setNoDelay(noDelay = true) {
            // No-op: fetch handles TCP settings
            return this;
        }

        setSocketKeepAlive(enable = false, initialDelay = 0) {
            // No-op: fetch handles keep-alive
            return this;
        }

        getRawHeaderNames() {
            return Object.keys(this._headers);
        }

        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (this._ended || this._destroyed) {
                if (callback) callback();
                return this;
            }

            if (data !== null && data !== undefined) {
                this.write(data, encoding);
            }

            this._ended = true;
            this._finished = true;

            this._doRequest().then(() => {
                if (callback) callback();
            }).catch((err) => {
                this.emit('error', err);
                if (callback) callback(err);
            });

            return this;
        }

        async _doRequest() {
            if (this._destroyed) return;

            const fetchOptions = {
                method: this.method,
                headers: this._headers,
            };

            // Add body for non-GET/HEAD requests
            if (this._chunks.length > 0 && this.method !== 'GET' && this.method !== 'HEAD') {
                fetchOptions.body = this._chunks.join('');
            }

            try {
                // Emit socket event for compatibility
                this.emit('socket', this._socket || {});

                const response = await fetch(this._url, fetchOptions);

                // Create IncomingMessage for the response
                const incomingMessage = new IncomingMessage(response, true);
                this._response = incomingMessage;
                this.emit('response', incomingMessage);

                // Start reading the body asynchronously
                incomingMessage._startReading();
            } catch (err) {
                this.emit('error', err);
            }
        }
    }

    // ============================================
    // IncomingMessage - Incoming HTTP message
    // ============================================
    class IncomingMessage extends EventEmitter {
        constructor(source, isClientResponse = false) {
            super();

            this._source = source;
            this._isClientResponse = isClientResponse;
            this._complete = false;
            this._aborted = false;
            this._socket = null;
            this._body = null;
            this._bodyConsumed = false;
            this.readable = true;

            // Trailers
            this.trailers = {};
            this.trailersDistinct = {};
            this.rawTrailers = [];

            if (isClientResponse && source instanceof Response) {
                // Client response from fetch
                this.statusCode = source.status;
                this.statusMessage = source.statusText;
                this.httpVersion = '1.1';
                this.httpVersionMajor = 1;
                this.httpVersionMinor = 1;

                // Parse headers
                this.headers = {};
                this.headersDistinct = {};
                this.rawHeaders = [];
                source.headers.forEach((value, key) => {
                    const lowerKey = key.toLowerCase();
                    this.headers[lowerKey] = value;
                    // headersDistinct contains arrays
                    if (!this.headersDistinct[lowerKey]) {
                        this.headersDistinct[lowerKey] = [];
                    }
                    this.headersDistinct[lowerKey].push(value);
                    this.rawHeaders.push(key, value);
                });
            } else if (source && typeof source === 'object' && !isClientResponse) {
                // Server request from Otter.serve
                this.method = source.method || 'GET';
                this.url = source.url || '/';
                this.httpVersion = '1.1';
                this.httpVersionMajor = 1;
                this.httpVersionMinor = 1;

                this.headers = parseHeaders(source.headers);
                this.headersDistinct = {};
                for (const [key, value] of Object.entries(this.headers)) {
                    this.headersDistinct[key] = Array.isArray(value) ? value : [value];
                }
                this.rawHeaders = headersToRawHeaders(source.headers || {});
                this._body = source.body;
            } else {
                // Empty initialization
                this.method = null;
                this.url = null;
                this.headers = {};
                this.headersDistinct = {};
                this.rawHeaders = [];
                this.httpVersion = '1.1';
                this.httpVersionMajor = 1;
                this.httpVersionMinor = 1;
            }
        }

        get complete() { return this._complete; }
        get aborted() { return this._aborted; }
        get socket() { return this._socket; }
        get connection() { return this._socket; } // deprecated alias

        /**
         * Start reading the response body (for client responses)
         */
        async _startReading() {
            if (!this._source || !(this._source instanceof Response)) return;

            try {
                const reader = this._source.body?.getReader();
                if (!reader) {
                    this._complete = true;
                    this.readable = false;
                    this.emit('end');
                    this.emit('close');
                    return;
                }

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;

                    // Emit as Buffer if available, otherwise Uint8Array
                    const chunk = typeof Buffer !== 'undefined' ? Buffer.from(value) : value;
                    this.emit('data', chunk);
                }

                this._complete = true;
                this.readable = false;
                this.emit('end');
            } catch (err) {
                this.emit('error', err);
            } finally {
                this.emit('close');
            }
        }

        /**
         * Emit body events for server requests
         */
        _emitBody() {
            if (this._body && !this._bodyConsumed) {
                this._bodyConsumed = true;
                if (typeof this._body === 'string') {
                    const chunk = typeof Buffer !== 'undefined'
                        ? Buffer.from(this._body)
                        : new TextEncoder().encode(this._body);
                    this.emit('data', chunk);
                } else if (this._body instanceof Uint8Array) {
                    const chunk = typeof Buffer !== 'undefined'
                        ? Buffer.from(this._body)
                        : this._body;
                    this.emit('data', chunk);
                }
            }
            this._complete = true;
            this.readable = false;
            this.emit('end');
        }

        /**
         * Read the entire body as a string (convenience method)
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
         * Read the entire body as JSON (convenience method)
         */
        async json() {
            const text = await this.text();
            return JSON.parse(text);
        }

        setTimeout(msecs, callback) {
            if (callback) this.once('timeout', callback);
            return this;
        }

        destroy(error) {
            if (this._aborted) return this;
            this._aborted = true;
            this._complete = true;
            this.readable = false;
            if (error) this.emit('error', error);
            this.emit('close');
            return this;
        }
    }

    // ============================================
    // ServerResponse - extends OutgoingMessage
    // ============================================
    class ServerResponse extends OutgoingMessage {
        constructor(req) {
            super();

            this.req = req;
            this[kRequest] = req;
            this[kSocket] = req ? req[kSocket] : null;

            // Response properties
            this.statusCode = 200;
            this.statusMessage = '';
            this.sendDate = true;
            this.strictContentLength = false;
            this._bodyData = [];

            // For sendResponse callback
            this._resolve = null;
            this._responsePromise = new Promise((resolve) => {
                this._resolve = resolve;
            });
        }

        get finished() { return this._finished; }

        /**
         * Write status line and headers.
         */
        writeHead(statusCode, statusMessage, headers) {
            if (this._headersSent) {
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

            this._headersSent = true;
            return this;
        }

        /**
         * Send 100 Continue response
         */
        writeContinue() {
            // Not directly supported - would need native integration
        }

        /**
         * Send 102 Processing response
         */
        writeProcessing() {
            // Not directly supported - would need native integration
        }

        /**
         * Send 103 Early Hints response
         */
        writeEarlyHints(hints, callback) {
            // Not directly supported - would need native integration
            if (callback) queueMicrotask(callback);
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

            if (!this._headersSent) {
                this._headersSent = true;
            }

            if (chunk !== null && chunk !== undefined) {
                if (typeof chunk === 'string') {
                    this._bodyData.push(chunk);
                } else if (chunk instanceof Uint8Array) {
                    this._bodyData.push(new TextDecoder().decode(chunk));
                } else {
                    this._bodyData.push(String(chunk));
                }
            }

            if (callback) callback();
            return true;
        }

        /**
         * End the response.
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

            if (data !== null && data !== undefined) {
                this.write(data, encoding);
            }

            this._finished = true;
            this._headersSent = true;

            // Add Date header if sendDate is true
            if (this.sendDate && !this.hasHeader('date')) {
                this._headers['date'] = new Date().toUTCString();
            }

            // Resolve the response promise with the final response
            const body = this._bodyData.join('');
            const statusMessage = this.statusMessage || STATUS_CODES[this.statusCode] || '';

            this._resolve({
                status: this.statusCode,
                statusText: statusMessage,
                headers: this._headers,
                body: body,
            });

            this.emit('prefinish');
            this.emit('finish');
            this.emit('close');

            if (callback) callback();
            return this;
        }
    }

    // ============================================
    // Server - HTTP Server
    // ============================================
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
            this.requestTimeout = options.requestTimeout || 300000;
            this.maxRequestsPerSocket = options.maxRequestsPerSocket || 0;

            if (requestListener) {
                this.on('request', requestListener);
            }
        }

        get listening() {
            return this._listening;
        }

        /**
         * Start listening for connections.
         */
        listen(port, host, backlog, callback) {
            // Normalize arguments
            if (typeof port === 'object' && port !== null) {
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
            const req = new IncomingMessage(requestData, false);
            const res = new ServerResponse(req);

            // Emit connection event (simplified)
            this._connections++;
            this.emit('connection', {});

            // Check for Expect: 100-continue
            if (req.headers['expect'] === '100-continue') {
                this.emit('checkContinue', req, res);
            } else if (req.headers['expect']) {
                this.emit('checkExpectation', req, res);
            } else {
                // Emit request event
                this.emit('request', req, res);
            }

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
         * Close all connections immediately.
         */
        closeAllConnections() {
            // Not directly supported
        }

        /**
         * Close idle connections.
         */
        closeIdleConnections() {
            // Not directly supported
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

        setTimeout(msecs, callback) {
            this.timeout = msecs;
            if (callback) this.on('timeout', callback);
            return this;
        }

        /**
         * Async dispose support
         */
        [Symbol.asyncDispose]() {
            return new Promise((resolve) => this.close(resolve));
        }
    }

    // ============================================
    // Module Functions
    // ============================================

    /**
     * Create an HTTP server.
     */
    function createServer(options, requestListener) {
        return new Server(options, requestListener);
    }

    /**
     * Make an HTTP request.
     */
    function request(urlOrOptions, optionsOrCallback, callback) {
        let options = urlOrOptions;

        if (typeof urlOrOptions === 'string' || urlOrOptions instanceof URL) {
            const url = typeof urlOrOptions === 'string' ? new URL(urlOrOptions) : urlOrOptions;
            options = {
                protocol: url.protocol,
                hostname: url.hostname,
                port: url.port || (url.protocol === 'https:' ? 443 : 80),
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

        // Ensure http protocol for http module
        if (!options.protocol) {
            options.protocol = 'http:';
        }

        return new ClientRequest(options, callback);
    }

    /**
     * Make an HTTP GET request.
     */
    function get(urlOrOptions, optionsOrCallback, callback) {
        let options = urlOrOptions;

        if (typeof urlOrOptions === 'string' || urlOrOptions instanceof URL) {
            const url = typeof urlOrOptions === 'string' ? new URL(urlOrOptions) : urlOrOptions;
            options = {
                protocol: url.protocol,
                hostname: url.hostname,
                port: url.port || (url.protocol === 'https:' ? 443 : 80),
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

        if (!options.protocol) {
            options.protocol = 'http:';
        }

        const req = new ClientRequest(options, callback);
        req.end();
        return req;
    }

    // ============================================
    // Module Export
    // ============================================
    const httpModule = {
        // Classes
        Agent,
        ClientRequest,
        IncomingMessage,
        OutgoingMessage,
        Server,
        ServerResponse,

        // Factory functions
        createServer,
        request,
        get,

        // Utilities
        validateHeaderName,
        validateHeaderValue,
        setMaxIdleHTTPParsers,

        // Constants
        STATUS_CODES,
        METHODS,
        maxHeaderSize,
        globalAgent,
    };

    // Add default export
    httpModule.default = httpModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('http', httpModule);
    }
})();
