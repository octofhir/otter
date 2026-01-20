/**
 * node:tls - TLS/SSL module for Node.js compatibility.
 *
 * Provides TLS/SSL encrypted TCP connections using rustls.
 * Uses #[dive] native functions for actual I/O operations.
 */
(function() {
    'use strict';

    // Get EventEmitter from the runtime
    const { EventEmitter } = globalThis.__otter_get_node_builtin('events');

    // Symbol for internal handle
    const kHandle = Symbol('handle');
    const kAuthorized = Symbol('authorized');
    const kAuthorizationError = Symbol('authorizationError');

    function toBase64(data) {
        return Buffer.from(data).toString('base64');
    }

    function fromBase64(base64) {
        return Buffer.from(base64, 'base64');
    }

    /**
     * Represents a TLS socket connection.
     * @extends EventEmitter
     *
     * Events:
     * - 'secureConnect' - TLS handshake completed
     * - 'secureConnection' - Alias for 'secureConnect'
     * - 'data' - Data received (Buffer)
     * - 'end' - Remote end closed write side
     * - 'close' - Socket fully closed
     * - 'error' - Error occurred
     * - 'drain' - Write buffer drained
     */
    class TLSSocket extends EventEmitter {
        constructor(options) {
            super();
            this[kHandle] = null;
            this.connecting = false;
            this.destroyed = false;
            this.readable = true;
            this.writable = true;
            this.authorized = false;
            this.authorizationError = null;

            if (options && options.servername) {
                this.servername = options.servername;
            }
        }

        /**
         * Connect to a remote TLS server.
         * @param {Object|number} options - Port number or options object
         * @param {string} [host] - Host to connect to
         * @param {Function} [callback] - 'secureConnect' event listener
         * @returns {TLSSocket} this
         */
        connect(options, host, callback) {
            if (typeof options === 'number') {
                options = { port: options, host: host || 'localhost' };
            }

            if (typeof host === 'function') {
                callback = host;
                host = options.host || 'localhost';
            }

            const port = options.port;
            host = options.host || host || 'localhost';

            if (callback) {
                this.once('secureConnect', callback);
            }

            this.connecting = true;

            // Normalize options
            const connectOptions = {
                port: port,
                host: host,
                rejectUnauthorized: options.rejectUnauthorized !== false,
                ca: options.ca,
                cert: options.cert,
                key: options.key,
                servername: options.servername || host
            };

            tls_connect(connectOptions).then(socketId => {
                this[kHandle] = socketId;
                this.connecting = false;
                tlsSocketRegistry.set(socketId, this);
            }).catch(err => {
                this.connecting = false;
                this.authorizationError = err.message || String(err);
                this[kAuthorizationError] = err.message || String(err);
                this.emit('error', new Error(err.message || String(err)));
            });

            return this;
        }

        /**
         * Write data to the TLS socket.
         * @param {string|Buffer} data - Data to write
         * @param {string} [encoding='utf8'] - Encoding if data is string
         * @param {Function} [callback] - Called when data is written
         * @returns {boolean} - Whether the data was flushed
         */
        write(data, encoding, callback) {
            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = 'utf8';
            }

            if (this.destroyed || !this[kHandle]) {
                const err = new Error('Socket is closed');
                if (callback) callback(err);
                return false;
            }

            try {
                if (typeof data === 'string') {
                    tls_socket_write_string(this[kHandle], data);
                } else if (data instanceof Uint8Array || ArrayBuffer.isView(data)) {
                    tls_socket_write(this[kHandle], toBase64(data));
                } else {
                    throw new Error('Invalid data type');
                }

                if (callback) callback();
                return true;
            } catch (err) {
                if (callback) callback(err);
                return false;
            }
        }

        /**
         * Half-close the socket (send FIN).
         * @param {string|Buffer} [data] - Final data to write
         * @param {string} [encoding] - Encoding
         * @param {Function} [callback] - Called when socket is ended
         * @returns {TLSSocket} this
         */
        end(data, encoding, callback) {
            if (typeof data === 'function') {
                callback = data;
                data = undefined;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = undefined;
            }

            if (data) {
                this.write(data, encoding);
            }

            if (this[kHandle]) {
                try {
                    tls_socket_end(this[kHandle]);
                } catch (e) {
                    // Ignore errors on end
                }
            }

            this.writable = false;

            if (callback) {
                this.once('close', callback);
            }

            return this;
        }

        /**
         * Destroy the socket immediately.
         * @param {Error} [error] - Error to emit
         * @returns {TLSSocket} this
         */
        destroy(error) {
            if (this.destroyed) return this;

            this.destroyed = true;
            this.readable = false;
            this.writable = false;

            if (this[kHandle]) {
                try {
                    tls_socket_destroy(this[kHandle]);
                } catch (e) {
                    // Ignore errors on destroy
                }
                tlsSocketRegistry.delete(this[kHandle]);
                this[kHandle] = null;
            }

            if (error) {
                this.emit('error', error);
            }

            // Close event will be emitted by the native side
            return this;
        }

        get authorized() {
            return this[kAuthorized] || false;
        }

        set authorized(value) {
            this[kAuthorized] = value;
        }

        get authorizationError() {
            return this[kAuthorizationError] || null;
        }

        set authorizationError(value) {
            this[kAuthorizationError] = value;
        }
    }

    // Socket registry for event routing
    const tlsSocketRegistry = new Map();

    /**
     * Handle events from native side.
     * This is called by the runtime when TLS events occur.
     */
    function handleTlsEvent(event) {
        switch (event.type) {
            case 'socketConnect': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    socket.authorized = true;
                    socket[kAuthorized] = true;
                    socket.emit('connect');
                    socket.emit('secureConnect');
                    socket.emit('secureConnection');
                }
                break;
            }
            case 'socketData': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    socket.emit('data', fromBase64(event.data));
                }
                break;
            }
            case 'socketEnd': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    socket.readable = false;
                    socket.emit('end');
                }
                break;
            }
            case 'socketClose': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    tlsSocketRegistry.delete(event.socketId);
                    socket.destroyed = true;
                    socket.readable = false;
                    socket.writable = false;
                    socket.emit('close', event.hadError);
                }
                break;
            }
            case 'socketError': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    socket.authorizationError = event.error;
                    socket.authorized = false;
                    socket.emit('error', new Error(event.error));
                }
                break;
            }
            case 'socketDrain': {
                const socket = tlsSocketRegistry.get(event.socketId);
                if (socket) {
                    socket.emit('drain');
                }
                break;
            }
        }
    }

    /**
     * Create a new TLS connection to a server.
     * @param {Object|number} options - Port or connection options
     * @param {string} [host] - Host to connect to
     * @param {Function} [callback] - 'secureConnect' event listener
     * @returns {TLSSocket}
     */
    function connect(options, host, callback) {
        const socket = new TLSSocket();

        return socket.connect(options, host, callback);
    }

    /**
     * Check if a value is a valid IP address.
     * @param {string} ip
     * @returns {number} - 4 for IPv4, 6 for IPv6, 0 for invalid
     */
    function isIP(ip) {
        if (typeof ip !== 'string') return 0;
        if (/^(\d{1,3}\.){3}\d{1,3}$/.test(ip)) {
            const parts = ip.split('.').map(Number);
            if (parts.every(p => p >= 0 && p <= 255)) return 4;
        }
        if (/^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$/.test(ip)) return 6;
        return 0;
    }

    function isIPv4(ip) { return isIP(ip) === 4; }
    function isIPv6(ip) { return isIP(ip) === 6; }

    // TLS module
    const tlsModule = {
        TLSSocket,
        connect,
        isIP,
        isIPv4,
        isIPv6,
        // Internal: event handler for native events
        __handleTlsEvent: handleTlsEvent,
    };

    // Add default export
    tlsModule.default = tlsModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('tls', tlsModule);
    }

    if (typeof globalThis.__otter_register_refed_checker === 'function') {
        globalThis.__otter_register_refed_checker(() => tlsSocketRegistry.size);
    }

    // Register global dispatch function for native events
    // The runtime calls __otter_tls_dispatch(jsonString) to deliver events
    globalThis.__otter_tls_dispatch = (eventJson) => {
        try {
            const event = JSON.parse(eventJson);
            handleTlsEvent(event);
        } catch (e) {
            console.error('Error handling TLS event:', e);
        }
    };

    // Hook into net dispatch to route TLS events delivered via net channel.
    if (typeof globalThis.__otter_net_dispatch === 'function') {
        const originalNetDispatch = globalThis.__otter_net_dispatch;
        globalThis.__otter_net_dispatch = (eventJson) => {
            originalNetDispatch(eventJson);
            try {
                const event = JSON.parse(eventJson);
                handleTlsEvent(event);
            } catch (e) {
                console.error('Error handling TLS net event:', e);
            }
        };
    }
})();
