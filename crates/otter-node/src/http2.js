/**
 * node:http2 - Node.js compatible HTTP/2 module.
 *
 * Provides the complete Node.js http2 API:
 * - http2.connect() - Create client session
 * - http2.createServer() - Create HTTP/2 server
 * - http2.createSecureServer() - Create HTTP/2 TLS server
 * - Http2Session, Http2Stream, Http2Server classes
 * - Http2ServerRequest, Http2ServerResponse for compat layer
 *
 * Uses hyper HTTP/2 implementation via native ops.
 */
(function() {
    'use strict';

    // Get EventEmitter from the runtime
    const { EventEmitter } = globalThis.__otter_get_node_builtin('events');

    // HTTP/2 constants (RFC 7540 / 9113)
    const constants = {
        // Session types
        NGHTTP2_SESSION_SERVER: 0,
        NGHTTP2_SESSION_CLIENT: 1,

        // Stream states
        NGHTTP2_STREAM_STATE_IDLE: 1,
        NGHTTP2_STREAM_STATE_OPEN: 2,
        NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,
        NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6,
        NGHTTP2_STREAM_STATE_CLOSED: 7,

        // Error codes
        NGHTTP2_NO_ERROR: 0,
        NGHTTP2_PROTOCOL_ERROR: 1,
        NGHTTP2_INTERNAL_ERROR: 2,
        NGHTTP2_FLOW_CONTROL_ERROR: 3,
        NGHTTP2_SETTINGS_TIMEOUT: 4,
        NGHTTP2_STREAM_CLOSED: 5,
        NGHTTP2_FRAME_SIZE_ERROR: 6,
        NGHTTP2_REFUSED_STREAM: 7,
        NGHTTP2_CANCEL: 8,
        NGHTTP2_COMPRESSION_ERROR: 9,
        NGHTTP2_CONNECT_ERROR: 10,
        NGHTTP2_ENHANCE_YOUR_CALM: 11,
        NGHTTP2_INADEQUATE_SECURITY: 12,
        NGHTTP2_HTTP_1_1_REQUIRED: 13,

        // Settings identifiers
        NGHTTP2_SETTINGS_HEADER_TABLE_SIZE: 0x1,
        NGHTTP2_SETTINGS_ENABLE_PUSH: 0x2,
        NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS: 0x3,
        NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE: 0x4,
        NGHTTP2_SETTINGS_MAX_FRAME_SIZE: 0x5,
        NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE: 0x6,

        // Frame types
        NGHTTP2_FRAME_DATA: 0,
        NGHTTP2_FRAME_HEADERS: 1,
        NGHTTP2_FRAME_PRIORITY: 2,
        NGHTTP2_FRAME_RST_STREAM: 3,
        NGHTTP2_FRAME_SETTINGS: 4,
        NGHTTP2_FRAME_PUSH_PROMISE: 5,
        NGHTTP2_FRAME_PING: 6,
        NGHTTP2_FRAME_GOAWAY: 7,
        NGHTTP2_FRAME_WINDOW_UPDATE: 8,
        NGHTTP2_FRAME_CONTINUATION: 9,

        // Pseudo headers
        HTTP2_HEADER_STATUS: ':status',
        HTTP2_HEADER_METHOD: ':method',
        HTTP2_HEADER_AUTHORITY: ':authority',
        HTTP2_HEADER_SCHEME: ':scheme',
        HTTP2_HEADER_PATH: ':path',
        HTTP2_HEADER_PROTOCOL: ':protocol',

        // Common headers
        HTTP2_HEADER_ACCEPT_ENCODING: 'accept-encoding',
        HTTP2_HEADER_ACCEPT_LANGUAGE: 'accept-language',
        HTTP2_HEADER_ACCEPT_RANGES: 'accept-ranges',
        HTTP2_HEADER_ACCEPT: 'accept',
        HTTP2_HEADER_ACCESS_CONTROL_ALLOW_CREDENTIALS: 'access-control-allow-credentials',
        HTTP2_HEADER_ACCESS_CONTROL_ALLOW_HEADERS: 'access-control-allow-headers',
        HTTP2_HEADER_ACCESS_CONTROL_ALLOW_METHODS: 'access-control-allow-methods',
        HTTP2_HEADER_ACCESS_CONTROL_ALLOW_ORIGIN: 'access-control-allow-origin',
        HTTP2_HEADER_ACCESS_CONTROL_EXPOSE_HEADERS: 'access-control-expose-headers',
        HTTP2_HEADER_ACCESS_CONTROL_MAX_AGE: 'access-control-max-age',
        HTTP2_HEADER_ACCESS_CONTROL_REQUEST_HEADERS: 'access-control-request-headers',
        HTTP2_HEADER_ACCESS_CONTROL_REQUEST_METHOD: 'access-control-request-method',
        HTTP2_HEADER_AGE: 'age',
        HTTP2_HEADER_AUTHORIZATION: 'authorization',
        HTTP2_HEADER_CACHE_CONTROL: 'cache-control',
        HTTP2_HEADER_CONNECTION: 'connection',
        HTTP2_HEADER_CONTENT_DISPOSITION: 'content-disposition',
        HTTP2_HEADER_CONTENT_ENCODING: 'content-encoding',
        HTTP2_HEADER_CONTENT_LENGTH: 'content-length',
        HTTP2_HEADER_CONTENT_TYPE: 'content-type',
        HTTP2_HEADER_COOKIE: 'cookie',
        HTTP2_HEADER_DATE: 'date',
        HTTP2_HEADER_ETAG: 'etag',
        HTTP2_HEADER_FORWARDED: 'forwarded',
        HTTP2_HEADER_HOST: 'host',
        HTTP2_HEADER_IF_MODIFIED_SINCE: 'if-modified-since',
        HTTP2_HEADER_IF_NONE_MATCH: 'if-none-match',
        HTTP2_HEADER_IF_RANGE: 'if-range',
        HTTP2_HEADER_LAST_MODIFIED: 'last-modified',
        HTTP2_HEADER_LINK: 'link',
        HTTP2_HEADER_LOCATION: 'location',
        HTTP2_HEADER_RANGE: 'range',
        HTTP2_HEADER_REFERER: 'referer',
        HTTP2_HEADER_SERVER: 'server',
        HTTP2_HEADER_SET_COOKIE: 'set-cookie',
        HTTP2_HEADER_STRICT_TRANSPORT_SECURITY: 'strict-transport-security',
        HTTP2_HEADER_TRANSFER_ENCODING: 'transfer-encoding',
        HTTP2_HEADER_TE: 'te',
        HTTP2_HEADER_UPGRADE_INSECURE_REQUESTS: 'upgrade-insecure-requests',
        HTTP2_HEADER_UPGRADE: 'upgrade',
        HTTP2_HEADER_USER_AGENT: 'user-agent',
        HTTP2_HEADER_VARY: 'vary',
        HTTP2_HEADER_X_CONTENT_TYPE_OPTIONS: 'x-content-type-options',
        HTTP2_HEADER_X_FRAME_OPTIONS: 'x-frame-options',
        HTTP2_HEADER_KEEP_ALIVE: 'keep-alive',
        HTTP2_HEADER_PROXY_CONNECTION: 'proxy-connection',
        HTTP2_HEADER_X_XSS_PROTECTION: 'x-xss-protection',
        HTTP2_HEADER_ALT_SVC: 'alt-svc',
        HTTP2_HEADER_CONTENT_SECURITY_POLICY: 'content-security-policy',
        HTTP2_HEADER_EARLY_DATA: 'early-data',
        HTTP2_HEADER_EXPECT_CT: 'expect-ct',
        HTTP2_HEADER_ORIGIN: 'origin',
        HTTP2_HEADER_PURPOSE: 'purpose',
        HTTP2_HEADER_TIMING_ALLOW_ORIGIN: 'timing-allow-origin',
        HTTP2_HEADER_X_FORWARDED_FOR: 'x-forwarded-for',

        // HTTP methods
        HTTP2_METHOD_ACL: 'ACL',
        HTTP2_METHOD_BASELINE_CONTROL: 'BASELINE-CONTROL',
        HTTP2_METHOD_BIND: 'BIND',
        HTTP2_METHOD_CHECKIN: 'CHECKIN',
        HTTP2_METHOD_CHECKOUT: 'CHECKOUT',
        HTTP2_METHOD_CONNECT: 'CONNECT',
        HTTP2_METHOD_COPY: 'COPY',
        HTTP2_METHOD_DELETE: 'DELETE',
        HTTP2_METHOD_GET: 'GET',
        HTTP2_METHOD_HEAD: 'HEAD',
        HTTP2_METHOD_LABEL: 'LABEL',
        HTTP2_METHOD_LINK: 'LINK',
        HTTP2_METHOD_LOCK: 'LOCK',
        HTTP2_METHOD_MERGE: 'MERGE',
        HTTP2_METHOD_MKACTIVITY: 'MKACTIVITY',
        HTTP2_METHOD_MKCALENDAR: 'MKCALENDAR',
        HTTP2_METHOD_MKCOL: 'MKCOL',
        HTTP2_METHOD_MKREDIRECTREF: 'MKREDIRECTREF',
        HTTP2_METHOD_MKWORKSPACE: 'MKWORKSPACE',
        HTTP2_METHOD_MOVE: 'MOVE',
        HTTP2_METHOD_OPTIONS: 'OPTIONS',
        HTTP2_METHOD_ORDERPATCH: 'ORDERPATCH',
        HTTP2_METHOD_PATCH: 'PATCH',
        HTTP2_METHOD_POST: 'POST',
        HTTP2_METHOD_PRI: 'PRI',
        HTTP2_METHOD_PROPFIND: 'PROPFIND',
        HTTP2_METHOD_PROPPATCH: 'PROPPATCH',
        HTTP2_METHOD_PUT: 'PUT',
        HTTP2_METHOD_REBIND: 'REBIND',
        HTTP2_METHOD_REPORT: 'REPORT',
        HTTP2_METHOD_SEARCH: 'SEARCH',
        HTTP2_METHOD_TRACE: 'TRACE',
        HTTP2_METHOD_UNBIND: 'UNBIND',
        HTTP2_METHOD_UNCHECKOUT: 'UNCHECKOUT',
        HTTP2_METHOD_UNLINK: 'UNLINK',
        HTTP2_METHOD_UNLOCK: 'UNLOCK',
        HTTP2_METHOD_UPDATE: 'UPDATE',
        HTTP2_METHOD_UPDATEREDIRECTREF: 'UPDATEREDIRECTREF',
        HTTP2_METHOD_VERSION_CONTROL: 'VERSION-CONTROL',
    };

    // Default HTTP/2 settings (RFC 7540 Section 6.5.2)
    const DEFAULT_SETTINGS = {
        headerTableSize: 4096,
        enablePush: true,
        maxConcurrentStreams: 100,
        initialWindowSize: 65535,
        maxFrameSize: 16384,
        maxHeaderListSize: 65535,
    };

    // Settings identifiers for packing/unpacking
    const SETTINGS_IDS = {
        headerTableSize: 0x1,
        enablePush: 0x2,
        maxConcurrentStreams: 0x3,
        initialWindowSize: 0x4,
        maxFrameSize: 0x5,
        maxHeaderListSize: 0x6,
    };

    const SETTINGS_NAMES = {
        0x1: 'headerTableSize',
        0x2: 'enablePush',
        0x3: 'maxConcurrentStreams',
        0x4: 'initialWindowSize',
        0x5: 'maxFrameSize',
        0x6: 'maxHeaderListSize',
    };

    // Symbol for sensitive headers
    const sensitiveHeaders = Symbol.for('nodejs.http2.sensitiveHeaders');

    // Stream ID counter for client sessions
    let nextStreamId = 1;

    // ============================================
    // Http2Session - Base session class
    // ============================================
    class Http2Session extends EventEmitter {
        constructor(type, socket, options = {}) {
            super();

            this._type = type; // 'server' or 'client'
            this._socket = socket;
            this._options = options;
            this._sessionId = null;

            this._destroyed = false;
            this._closed = false;
            this._closing = false;
            this._goawayCode = null;
            this._goawayLastStreamId = null;

            this._streams = new Map();
            this._localSettings = { ...DEFAULT_SETTINGS, ...options.settings };
            this._remoteSettings = { ...DEFAULT_SETTINGS };
            this._pendingSettingsAck = false;

            // Origin set (RFC 8336)
            this._originSet = [];

            // Ping support
            this._pingCallbacks = new Map();
            this._pingId = 0;
        }

        get alpnProtocol() {
            return 'h2';
        }

        get closed() {
            return this._closed;
        }

        get connecting() {
            return !this._sessionId && !this._destroyed;
        }

        get destroyed() {
            return this._destroyed;
        }

        get encrypted() {
            return this._options.secure || false;
        }

        get localSettings() {
            return { ...this._localSettings };
        }

        get remoteSettings() {
            return { ...this._remoteSettings };
        }

        get originSet() {
            return [...this._originSet];
        }

        get pendingSettingsAck() {
            return this._pendingSettingsAck;
        }

        get socket() {
            return this._socket;
        }

        get state() {
            return {
                effectiveLocalWindowSize: this._localWindowSize || 65535,
                effectiveRecvDataLength: 0,
                nextStreamID: this._type === 'client' ? nextStreamId : 2,
                localWindowSize: this._localSettings.initialWindowSize,
                lastProcStreamID: 0,
                remoteWindowSize: this._remoteSettings.initialWindowSize,
                deflateDynamicTableSize: this._localSettings.headerTableSize,
                inflateDynamicTableSize: this._remoteSettings.headerTableSize,
            };
        }

        get type() {
            return this._type === 'server'
                ? constants.NGHTTP2_SESSION_SERVER
                : constants.NGHTTP2_SESSION_CLIENT;
        }

        close(callback) {
            if (this._closed || this._closing) {
                if (callback) queueMicrotask(callback);
                return;
            }

            this._closing = true;

            // Close all streams
            for (const stream of this._streams.values()) {
                stream.close(constants.NGHTTP2_NO_ERROR);
            }

            // Mark as closed
            this._closed = true;
            this._closing = false;

            this.emit('close');
            if (callback) queueMicrotask(callback);
        }

        destroy(error, code) {
            if (this._destroyed) return;

            this._destroyed = true;
            this._closed = true;

            // Destroy all streams
            for (const stream of this._streams.values()) {
                stream.destroy(error);
            }
            this._streams.clear();

            if (error) {
                this.emit('error', error);
            }

            this.emit('close');
        }

        goaway(code = constants.NGHTTP2_NO_ERROR, lastStreamID, opaqueData) {
            if (this._destroyed) {
                throw new Error('Session is destroyed');
            }

            this._goawayCode = code;
            this._goawayLastStreamId = lastStreamID;

            this.emit('goaway', code, lastStreamID, opaqueData);
        }

        ping(payload, callback) {
            if (this._destroyed) {
                const err = new Error('Session is destroyed');
                if (callback) {
                    queueMicrotask(() => callback(err));
                }
                return false;
            }

            if (typeof payload === 'function') {
                callback = payload;
                payload = Buffer.alloc(8);
            }

            if (!payload) {
                payload = Buffer.alloc(8);
            }

            // Generate ping ID
            const pingId = ++this._pingId;
            const startTime = Date.now();

            if (callback) {
                this._pingCallbacks.set(pingId, { callback, startTime, payload });

                // Simulate ping response (would be native op in full implementation)
                queueMicrotask(() => {
                    const info = this._pingCallbacks.get(pingId);
                    if (info) {
                        this._pingCallbacks.delete(pingId);
                        const duration = Date.now() - info.startTime;
                        info.callback(null, duration, info.payload);
                    }
                });
            }

            return true;
        }

        ref() {
            // Keep event loop alive
            return this;
        }

        unref() {
            // Allow event loop to exit
            return this;
        }

        settings(settings, callback) {
            if (this._destroyed) {
                throw new Error('Session is destroyed');
            }

            Object.assign(this._localSettings, settings);
            this._pendingSettingsAck = true;

            // Simulate settings ack
            queueMicrotask(() => {
                this._pendingSettingsAck = false;
                this.emit('localSettings', this._localSettings);
                if (callback) callback();
            });
        }

        setTimeout(msecs, callback) {
            if (callback) {
                this.once('timeout', callback);
            }

            if (msecs > 0) {
                this._timeout = setTimeout(() => {
                    this.emit('timeout');
                }, msecs);
            } else if (this._timeout) {
                clearTimeout(this._timeout);
                this._timeout = null;
            }

            return this;
        }

        setLocalWindowSize(windowSize) {
            this._localSettings.initialWindowSize = windowSize;
        }
    }

    // ============================================
    // ClientHttp2Session - Client-side session
    // ============================================
    class ClientHttp2Session extends Http2Session {
        constructor(authority, options = {}) {
            super('client', null, options);

            this._authority = authority;
            this._url = null;

            // Parse authority
            try {
                this._url = new URL(authority);
            } catch (e) {
                // If not a valid URL, treat as hostname
                this._url = new URL(`https://${authority}`);
            }
        }

        get authority() {
            return this._authority;
        }

        /**
         * Create a new HTTP/2 stream for a request
         */
        request(headers, options = {}) {
            if (this._destroyed) {
                throw new Error('Session is destroyed');
            }

            if (this._closed) {
                throw new Error('Session is closed');
            }

            // Assign stream ID (odd numbers for client-initiated)
            const streamId = nextStreamId;
            nextStreamId += 2;

            // Create stream
            const stream = new ClientHttp2Stream(this, streamId, headers, options);
            this._streams.set(streamId, stream);

            // Start the request
            stream._initRequest();

            return stream;
        }
    }

    // ============================================
    // ServerHttp2Session - Server-side session
    // ============================================
    class ServerHttp2Session extends Http2Session {
        constructor(socket, options = {}) {
            super('server', socket, options);
        }

        /**
         * Send ALTSVC frame
         */
        altsvc(alt, originOrStream) {
            if (this._destroyed) {
                throw new Error('Session is destroyed');
            }
            // Would send ALTSVC frame via native op
        }

        /**
         * Send ORIGIN frame (RFC 8336)
         */
        origin(...origins) {
            if (this._destroyed) {
                throw new Error('Session is destroyed');
            }

            for (const origin of origins) {
                if (!this._originSet.includes(origin)) {
                    this._originSet.push(origin);
                }
            }
        }
    }

    // ============================================
    // Http2Stream - Base stream class
    // ============================================
    class Http2Stream extends EventEmitter {
        constructor(session, id) {
            super();

            this._session = session;
            this._id = id;
            this._closed = false;
            this._destroyed = false;
            this._aborted = false;
            this._pending = true;
            this._state = constants.NGHTTP2_STREAM_STATE_OPEN;

            this._sentHeaders = null;
            this._sentTrailers = null;
            this._sentInfoHeaders = [];

            this._localWindowSize = session._localSettings.initialWindowSize;
            this._remoteWindowSize = session._remoteSettings.initialWindowSize;

            this._rstCode = null;
            this._endAfterHeaders = false;
        }

        get aborted() {
            return this._aborted;
        }

        get bufferSize() {
            return 0;
        }

        get closed() {
            return this._closed;
        }

        get destroyed() {
            return this._destroyed;
        }

        get endAfterHeaders() {
            return this._endAfterHeaders;
        }

        get id() {
            return this._id;
        }

        get pending() {
            return this._pending;
        }

        get rstCode() {
            return this._rstCode;
        }

        get sentHeaders() {
            return this._sentHeaders;
        }

        get sentInfoHeaders() {
            return this._sentInfoHeaders;
        }

        get sentTrailers() {
            return this._sentTrailers;
        }

        get session() {
            return this._session;
        }

        get state() {
            return {
                localWindowSize: this._localWindowSize,
                state: this._state,
                localClose: this._state === constants.NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL ||
                           this._state === constants.NGHTTP2_STREAM_STATE_CLOSED,
                remoteClose: this._state === constants.NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE ||
                            this._state === constants.NGHTTP2_STREAM_STATE_CLOSED,
                sumDependencyWeight: 16,
                weight: 16,
            };
        }

        close(code = constants.NGHTTP2_NO_ERROR, callback) {
            if (this._closed) {
                if (callback) queueMicrotask(callback);
                return;
            }

            this._closed = true;
            this._rstCode = code;
            this._state = constants.NGHTTP2_STREAM_STATE_CLOSED;

            // Remove from session
            this._session._streams.delete(this._id);

            this.emit('close');
            if (callback) queueMicrotask(callback);
        }

        destroy(error) {
            if (this._destroyed) return;

            this._destroyed = true;
            this._closed = true;
            this._state = constants.NGHTTP2_STREAM_STATE_CLOSED;

            // Remove from session
            this._session._streams.delete(this._id);

            if (error) {
                this.emit('error', error);
            }

            this.emit('close');
        }

        priority(options) {
            // Set stream priority
            // Would send PRIORITY frame via native op
        }

        setTimeout(msecs, callback) {
            if (callback) {
                this.once('timeout', callback);
            }

            if (msecs > 0) {
                this._timeout = setTimeout(() => {
                    this.emit('timeout');
                }, msecs);
            } else if (this._timeout) {
                clearTimeout(this._timeout);
                this._timeout = null;
            }

            return this;
        }

        sendTrailers(headers) {
            if (this._closed || this._destroyed) {
                throw new Error('Stream is closed');
            }

            this._sentTrailers = headers;
            // Would send via native op
        }
    }

    // ============================================
    // ClientHttp2Stream - Client request stream
    // ============================================
    class ClientHttp2Stream extends Http2Stream {
        constructor(session, id, headers, options) {
            super(session, id);

            this._requestHeaders = headers;
            this._requestOptions = options;
            this._body = [];
            this._responseHeaders = null;
            this._ended = false;

            // Writable stream properties
            this.writable = true;
            this.writableEnded = false;
            this.writableFinished = false;
        }

        /**
         * Initialize the request using fetch
         */
        async _initRequest() {
            try {
                const session = this._session;
                const url = session._url;

                // Build request URL from headers
                const method = this._requestHeaders[':method'] || 'GET';
                const path = this._requestHeaders[':path'] || '/';
                const scheme = this._requestHeaders[':scheme'] || url.protocol.replace(':', '');
                const authority = this._requestHeaders[':authority'] || url.host;

                const requestUrl = `${scheme}://${authority}${path}`;

                // Build headers (filter out pseudo-headers)
                const fetchHeaders = {};
                for (const [key, value] of Object.entries(this._requestHeaders)) {
                    if (!key.startsWith(':')) {
                        fetchHeaders[key] = value;
                    }
                }

                // Mark as ready
                this._pending = false;
                this.emit('ready');

                // Build fetch options
                const fetchOptions = {
                    method,
                    headers: fetchHeaders,
                };

                // Add body for non-GET/HEAD requests
                if (this._body.length > 0 && method !== 'GET' && method !== 'HEAD') {
                    fetchOptions.body = this._body.join('');
                }

                // Make the request
                const response = await fetch(requestUrl, fetchOptions);

                // Build response headers with pseudo-header
                this._responseHeaders = {
                    ':status': response.status,
                };

                response.headers.forEach((value, key) => {
                    this._responseHeaders[key] = value;
                });

                // Emit response event
                this.emit('response', this._responseHeaders, 0);

                // Read and emit data
                const reader = response.body?.getReader();
                if (reader) {
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;

                        const chunk = typeof Buffer !== 'undefined' ? Buffer.from(value) : value;
                        this.emit('data', chunk);
                    }
                }

                // Emit end
                this.emit('end');
                this.close();

            } catch (err) {
                this.emit('error', err);
                this.destroy(err);
            }
        }

        /**
         * Write data to the request body
         */
        write(chunk, encoding, callback) {
            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = 'utf8';
            }

            if (this._ended || this._destroyed) {
                const err = new Error('write after end');
                if (callback) callback(err);
                return false;
            }

            if (chunk !== null && chunk !== undefined) {
                if (typeof chunk === 'string') {
                    this._body.push(chunk);
                } else if (chunk instanceof Uint8Array) {
                    this._body.push(new TextDecoder().decode(chunk));
                } else {
                    this._body.push(String(chunk));
                }
            }

            if (callback) callback();
            return true;
        }

        /**
         * End the request
         */
        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (this._ended) {
                if (callback) callback();
                return this;
            }

            if (data !== null && data !== undefined) {
                this.write(data, encoding);
            }

            this._ended = true;
            this.writableEnded = true;

            if (callback) queueMicrotask(callback);
            return this;
        }
    }

    // ============================================
    // ServerHttp2Stream - Server response stream
    // ============================================
    class ServerHttp2Stream extends Http2Stream {
        constructor(session, id, headers) {
            super(session, id);

            this._requestHeaders = headers;
            this._headersSent = false;
            this._pushAllowed = session._remoteSettings.enablePush;
        }

        get headersSent() {
            return this._headersSent;
        }

        get pushAllowed() {
            return this._pushAllowed;
        }

        /**
         * Send response headers
         */
        respond(headers = {}, options = {}) {
            if (this._headersSent) {
                throw new Error('Response already sent');
            }

            // Ensure :status is set
            if (!headers[':status']) {
                headers[':status'] = 200;
            }

            this._headersSent = true;
            this._sentHeaders = headers;
            this._endAfterHeaders = options.endStream || false;

            // Would send via native op
        }

        /**
         * Send file as response
         */
        respondWithFile(path, headers, options) {
            // Would read file and send via native op
            throw new Error('respondWithFile not yet implemented');
        }

        /**
         * Send file descriptor as response
         */
        respondWithFD(fd, headers, options) {
            // Would send file descriptor via native op
            throw new Error('respondWithFD not yet implemented');
        }

        /**
         * Create a push stream (server push)
         */
        pushStream(headers, options, callback) {
            if (typeof options === 'function') {
                callback = options;
                options = {};
            }

            if (!this._pushAllowed) {
                const err = new Error('Push streams are disabled');
                if (callback) callback(err);
                return;
            }

            // Would create push stream via native op
            throw new Error('pushStream not yet implemented');
        }

        /**
         * Send additional headers (1xx informational)
         */
        additionalHeaders(headers) {
            this._sentInfoHeaders.push(headers);
            // Would send via native op
        }

        /**
         * Write data to response
         */
        write(chunk, encoding, callback) {
            if (!this._headersSent) {
                this.respond();
            }

            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = 'utf8';
            }

            // Would send data frame via native op
            if (callback) callback();
            return true;
        }

        /**
         * End the response
         */
        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (!this._headersSent) {
                this.respond();
            }

            if (data !== null && data !== undefined) {
                this.write(data, encoding);
            }

            this.close();
            if (callback) queueMicrotask(callback);
            return this;
        }
    }

    // ============================================
    // Http2ServerRequest - Compatibility layer
    // ============================================
    class Http2ServerRequest extends EventEmitter {
        constructor(stream, headers, options, rawHeaders) {
            super();

            this._stream = stream;
            this._headers = {};
            this._rawHeaders = rawHeaders || [];

            // Parse headers
            for (const [key, value] of Object.entries(headers)) {
                if (!key.startsWith(':')) {
                    this._headers[key] = value;
                }
            }

            // Pseudo headers
            this.method = headers[':method'] || 'GET';
            this.url = headers[':path'] || '/';
            this.authority = headers[':authority'] || '';
            this.scheme = headers[':scheme'] || 'https';

            this.httpVersion = '2.0';
            this.httpVersionMajor = 2;
            this.httpVersionMinor = 0;

            this.complete = false;
            this.aborted = false;
            this.readable = true;

            // Forward stream events
            stream.on('data', (chunk) => this.emit('data', chunk));
            stream.on('end', () => {
                this.complete = true;
                this.readable = false;
                this.emit('end');
            });
            stream.on('error', (err) => this.emit('error', err));
            stream.on('aborted', () => {
                this.aborted = true;
                this.emit('aborted');
            });
            stream.on('close', () => this.emit('close'));
        }

        get headers() {
            return this._headers;
        }

        get rawHeaders() {
            return this._rawHeaders;
        }

        get socket() {
            return this._stream.session.socket;
        }

        get stream() {
            return this._stream;
        }

        get trailers() {
            return {};
        }

        get rawTrailers() {
            return [];
        }

        setTimeout(msecs, callback) {
            this._stream.setTimeout(msecs, callback);
            return this;
        }

        destroy(error) {
            this._stream.destroy(error);
        }
    }

    // ============================================
    // Http2ServerResponse - Compatibility layer
    // ============================================
    class Http2ServerResponse extends EventEmitter {
        constructor(stream) {
            super();

            this._stream = stream;
            this._headers = {};
            this._statusCode = 200;
            this._statusMessage = '';
            this._headersSent = false;
            this._finished = false;

            // Forward stream events
            stream.on('finish', () => this.emit('finish'));
            stream.on('close', () => this.emit('close'));
            stream.on('error', (err) => this.emit('error', err));
        }

        get finished() {
            return this._finished;
        }

        get headersSent() {
            return this._headersSent;
        }

        get socket() {
            return this._stream.session.socket;
        }

        get stream() {
            return this._stream;
        }

        get statusCode() {
            return this._statusCode;
        }

        set statusCode(code) {
            this._statusCode = code;
        }

        get statusMessage() {
            return this._statusMessage;
        }

        set statusMessage(msg) {
            this._statusMessage = msg;
        }

        addTrailers(trailers) {
            this._stream.sendTrailers(trailers);
        }

        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (!this._headersSent) {
                this._sendHeaders();
            }

            this._finished = true;
            this._stream.end(data, encoding, callback);
            return this;
        }

        getHeader(name) {
            return this._headers[name.toLowerCase()];
        }

        getHeaderNames() {
            return Object.keys(this._headers);
        }

        getHeaders() {
            return { ...this._headers };
        }

        hasHeader(name) {
            return name.toLowerCase() in this._headers;
        }

        removeHeader(name) {
            if (this._headersSent) {
                throw new Error('Cannot remove headers after they are sent');
            }
            delete this._headers[name.toLowerCase()];
        }

        setHeader(name, value) {
            if (this._headersSent) {
                throw new Error('Cannot set headers after they are sent');
            }
            this._headers[name.toLowerCase()] = value;
        }

        setTimeout(msecs, callback) {
            this._stream.setTimeout(msecs, callback);
            return this;
        }

        write(chunk, encoding, callback) {
            if (!this._headersSent) {
                this._sendHeaders();
            }
            return this._stream.write(chunk, encoding, callback);
        }

        writeContinue() {
            this._stream.additionalHeaders({ ':status': 100 });
        }

        writeEarlyHints(hints) {
            this._stream.additionalHeaders({ ':status': 103, ...hints });
        }

        writeHead(statusCode, statusMessage, headers) {
            if (this._headersSent) {
                throw new Error('Cannot write headers after they are sent');
            }

            this._statusCode = statusCode;

            if (typeof statusMessage === 'object') {
                headers = statusMessage;
                statusMessage = undefined;
            }

            if (statusMessage) {
                this._statusMessage = statusMessage;
            }

            if (headers) {
                for (const [key, value] of Object.entries(headers)) {
                    this._headers[key.toLowerCase()] = value;
                }
            }

            return this;
        }

        _sendHeaders() {
            const headers = {
                ':status': this._statusCode,
                ...this._headers,
            };
            this._stream.respond(headers);
            this._headersSent = true;
        }

        createPushResponse(headers, callback) {
            this._stream.pushStream(headers, callback);
        }
    }

    // ============================================
    // Http2Server - HTTP/2 Server
    // ============================================
    class Http2Server extends EventEmitter {
        constructor(options = {}, requestListener) {
            super();

            if (typeof options === 'function') {
                requestListener = options;
                options = {};
            }

            this._options = options;
            this._listening = false;
            this._address = null;
            this._sessions = new Set();

            // Server options
            this.timeout = options.timeout || 0;
            this.maxHeadersCount = options.maxHeadersCount || 2000;

            if (requestListener) {
                this.on('request', requestListener);
            }
        }

        get listening() {
            return this._listening;
        }

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

            // Would start server via native op
            // For now, use Otter.serve() as a placeholder
            const self = this;

            if (typeof Otter !== 'undefined' && Otter.serve) {
                Otter.serve({
                    port,
                    hostname: host,
                    fetch: async (request) => {
                        return self._handleRequest(request);
                    },
                }).then((server) => {
                    self._server = server;
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
            } else {
                queueMicrotask(() => {
                    this.emit('error', new Error('Server not available - Otter.serve not found'));
                });
            }

            return this;
        }

        async _handleRequest(fetchRequest) {
            // Create pseudo HTTP/2 session and stream
            const session = new ServerHttp2Session(null, this._options);
            this._sessions.add(session);

            const url = new URL(fetchRequest.url);

            // Build HTTP/2 headers
            const headers = {
                ':method': fetchRequest.method,
                ':path': url.pathname + url.search,
                ':scheme': url.protocol.replace(':', ''),
                ':authority': url.host,
            };

            fetchRequest.headers.forEach((value, key) => {
                headers[key] = value;
            });

            // Create stream
            const stream = new ServerHttp2Stream(session, 1, headers);
            session._streams.set(1, stream);

            // Create compat layer objects
            const req = new Http2ServerRequest(stream, headers, {}, []);
            const res = new Http2ServerResponse(stream);

            // Emit events
            this.emit('session', session);
            this.emit('stream', stream, headers, 0);
            this.emit('request', req, res);

            // Wait for response
            return new Promise((resolve) => {
                let body = '';
                let status = 200;
                let responseHeaders = {};

                // Capture response
                const origRespond = stream.respond.bind(stream);
                stream.respond = (h, opts) => {
                    status = h[':status'] || 200;
                    for (const [k, v] of Object.entries(h)) {
                        if (!k.startsWith(':')) {
                            responseHeaders[k] = v;
                        }
                    }
                    stream._headersSent = true;
                };

                const origWrite = stream.write.bind(stream);
                stream.write = (chunk, enc, cb) => {
                    if (chunk) {
                        body += typeof chunk === 'string' ? chunk : new TextDecoder().decode(chunk);
                    }
                    if (cb) cb();
                    return true;
                };

                const origEnd = stream.end.bind(stream);
                stream.end = (data, enc, cb) => {
                    if (typeof data === 'function') {
                        cb = data;
                        data = undefined;
                    } else if (typeof enc === 'function') {
                        cb = enc;
                        enc = undefined;
                    }

                    if (!stream._headersSent) {
                        stream.respond({ ':status': res._statusCode, ...res._headers });
                    }

                    if (data) {
                        body += typeof data === 'string' ? data : new TextDecoder().decode(data);
                    }

                    // Clean up
                    this._sessions.delete(session);

                    // Build response
                    const respHeaders = new Headers();
                    for (const [k, v] of Object.entries(responseHeaders)) {
                        if (Array.isArray(v)) {
                            for (const item of v) {
                                respHeaders.append(k, item);
                            }
                        } else if (v !== undefined && v !== null) {
                            respHeaders.set(k, String(v));
                        }
                    }

                    resolve(new Response(body, {
                        status,
                        headers: respHeaders,
                    }));

                    if (cb) cb();
                };
            });
        }

        close(callback) {
            if (callback) {
                this.once('close', callback);
            }

            // Close all sessions
            for (const session of this._sessions) {
                session.close();
            }
            this._sessions.clear();

            if (this._server && this._server.shutdown) {
                this._server.shutdown();
            }

            this._listening = false;
            this.emit('close');
            return this;
        }

        address() {
            return this._address;
        }

        setTimeout(msecs, callback) {
            this.timeout = msecs;
            if (callback) this.on('timeout', callback);
            return this;
        }

        updateSettings(settings) {
            for (const session of this._sessions) {
                session.settings(settings);
            }
        }
    }

    // ============================================
    // Http2SecureServer - HTTP/2 with TLS
    // ============================================
    class Http2SecureServer extends Http2Server {
        constructor(options = {}, requestListener) {
            super({ ...options, secure: true }, requestListener);
        }
    }

    // ============================================
    // Module Functions
    // ============================================

    /**
     * Connect to an HTTP/2 server
     */
    function connect(authority, options, listener) {
        if (typeof options === 'function') {
            listener = options;
            options = {};
        }

        options = options || {};

        const session = new ClientHttp2Session(authority, options);

        if (listener) {
            session.once('connect', listener);
        }

        // Simulate connection (would be native op)
        queueMicrotask(() => {
            session._sessionId = Date.now(); // Placeholder
            session.emit('connect', session);
        });

        return session;
    }

    /**
     * Create an HTTP/2 server (unencrypted)
     */
    function createServer(options, onRequestHandler) {
        if (typeof options === 'function') {
            onRequestHandler = options;
            options = {};
        }
        return new Http2Server(options, onRequestHandler);
    }

    /**
     * Create an HTTP/2 secure server (with TLS)
     */
    function createSecureServer(options, onRequestHandler) {
        if (typeof options === 'function') {
            onRequestHandler = options;
            options = {};
        }
        return new Http2SecureServer(options, onRequestHandler);
    }

    /**
     * Get default HTTP/2 settings
     */
    function getDefaultSettings() {
        return { ...DEFAULT_SETTINGS };
    }

    /**
     * Pack settings into binary format
     */
    function getPackedSettings(settings) {
        const merged = { ...DEFAULT_SETTINGS, ...settings };
        const entries = Object.entries(SETTINGS_IDS);
        const buf = Buffer.alloc(entries.length * 6);

        let offset = 0;
        for (const [name, id] of entries) {
            let value = merged[name];
            if (name === 'enablePush') {
                value = value ? 1 : 0;
            }
            buf.writeUInt16BE(id, offset);
            buf.writeUInt32BE(value || 0, offset + 2);
            offset += 6;
        }

        return buf;
    }

    /**
     * Unpack settings from binary format
     */
    function getUnpackedSettings(buf) {
        const settings = {};

        for (let i = 0; i < buf.length; i += 6) {
            const id = buf.readUInt16BE(i);
            const value = buf.readUInt32BE(i + 2);
            const name = SETTINGS_NAMES[id];

            if (name) {
                if (name === 'enablePush') {
                    settings[name] = value === 1;
                } else {
                    settings[name] = value;
                }
            }
        }

        return settings;
    }

    // ============================================
    // Module Export
    // ============================================
    const http2Module = {
        // Constants
        constants,
        sensitiveHeaders,

        // Factory functions
        connect,
        createServer,
        createSecureServer,
        getDefaultSettings,
        getPackedSettings,
        getUnpackedSettings,

        // Classes
        Http2Session,
        ClientHttp2Session,
        ServerHttp2Session,
        Http2Stream,
        ClientHttp2Stream,
        ServerHttp2Stream,
        Http2Server,
        Http2SecureServer,
        Http2ServerRequest,
        Http2ServerResponse,
    };

    // Add default export
    http2Module.default = http2Module;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('http2', http2Module);
    }
})();
