/**
 * node:net - TCP networking module for Node.js compatibility.
 *
 * Provides net.Server, net.Socket, and factory functions for TCP networking.
 * Uses #[dive] native functions for actual I/O operations.
 */
(function() {
    'use strict';

    // Get EventEmitter from the runtime
    const { EventEmitter } = globalThis.__otter_get_node_builtin('events');

    // Symbol for internal handle
    const kHandle = Symbol('handle');
    const kServer = Symbol('server');

    /**
     * Represents a TCP socket connection.
     * @extends EventEmitter
     *
     * Events:
     * - 'connect' - Connection established
     * - 'data' - Data received (Buffer)
     * - 'end' - Remote end closed write side
     * - 'close' - Socket fully closed
     * - 'error' - Error occurred
     * - 'drain' - Write buffer drained
     */
    class Socket extends EventEmitter {
        constructor(options = {}) {
            super();
            this[kHandle] = null;
            this[kServer] = null;
            this.connecting = false;
            this.destroyed = false;
            this.readable = true;
            this.writable = true;
            this._bytesRead = 0;
            this._bytesWritten = 0;
            this._remoteAddress = null;
            this._remotePort = null;
            this._localAddress = null;
            this._localPort = null;
        }

        /**
         * Connect to a remote server.
         * @param {Object|number} options - Port number or options object
         * @param {string} [host='127.0.0.1'] - Host to connect to
         * @param {Function} [callback] - 'connect' event listener
         * @returns {Socket} this
         */
        connect(options, host, callback) {
            if (typeof options === 'number') {
                options = { port: options, host: host || '127.0.0.1' };
            } else if (typeof options === 'string') {
                // Unix socket path - not implemented yet
                throw new Error('Unix sockets not yet implemented');
            }

            if (typeof host === 'function') {
                callback = host;
                host = options.host || '127.0.0.1';
            }

            const port = options.port;
            host = options.host || host || '127.0.0.1';

            if (callback) {
                this.once('connect', callback);
            }

            this.connecting = true;

            // Call native connect
            net_connect(port, host).then(socketId => {
                this[kHandle] = socketId;
                this.connecting = false;
                this._updateAddressInfo();
                this.emit('connect');
            }).catch(err => {
                this.connecting = false;
                this.emit('error', new Error(err.message || String(err)));
            });

            return this;
        }

        /**
         * Write data to the socket.
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
                    net_socket_write_string(this[kHandle], data);
                } else if (data instanceof Uint8Array || ArrayBuffer.isView(data)) {
                    // Convert to base64 for transport
                    const base64 = btoa(String.fromCharCode.apply(null, new Uint8Array(data.buffer || data)));
                    net_socket_write(this[kHandle], base64);
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
         * @returns {Socket} this
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
                    net_socket_end(this[kHandle]);
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
         * @returns {Socket} this
         */
        destroy(error) {
            if (this.destroyed) return this;

            this.destroyed = true;
            this.readable = false;
            this.writable = false;

            if (this[kHandle]) {
                try {
                    net_socket_destroy(this[kHandle]);
                } catch (e) {
                    // Ignore errors on destroy
                }
                this[kHandle] = null;
            }

            if (error) {
                this.emit('error', error);
            }

            // Close event will be emitted by the native side
            return this;
        }

        /**
         * Set TCP_NODELAY option.
         * @param {boolean} [noDelay=true]
         * @returns {Socket} this
         */
        setNoDelay(noDelay = true) {
            if (this[kHandle]) {
                try {
                    net_set_no_delay(this[kHandle], noDelay);
                } catch (e) {
                    // Ignore
                }
            }
            return this;
        }

        /**
         * Set SO_KEEPALIVE option.
         * @param {boolean} [enable=false]
         * @param {number} [initialDelay=0]
         * @returns {Socket} this
         */
        setKeepAlive(enable = false, initialDelay = 0) {
            if (this[kHandle]) {
                try {
                    net_set_keep_alive(this[kHandle], enable);
                } catch (e) {
                    // Ignore
                }
            }
            return this;
        }

        /**
         * Set socket timeout.
         * @param {number} timeout - Timeout in milliseconds
         * @param {Function} [callback] - 'timeout' event listener
         * @returns {Socket} this
         */
        setTimeout(timeout, callback) {
            // TODO: Implement timeout
            if (callback) {
                this.once('timeout', callback);
            }
            return this;
        }

        /**
         * Pause reading data.
         * @returns {Socket} this
         */
        pause() {
            // TODO: Implement pause/resume
            return this;
        }

        /**
         * Resume reading data.
         * @returns {Socket} this
         */
        resume() {
            // TODO: Implement pause/resume
            return this;
        }

        _updateAddressInfo() {
            if (!this[kHandle]) return;
            try {
                const info = net_socket_info(this[kHandle]);
                this._remoteAddress = info.remoteAddress;
                this._remotePort = info.remotePort;
                this._localAddress = info.localAddress;
                this._localPort = info.localPort;
            } catch (e) {
                // Ignore
            }
        }

        get remoteAddress() { return this._remoteAddress; }
        get remotePort() { return this._remotePort; }
        get localAddress() { return this._localAddress; }
        get localPort() { return this._localPort; }
        get bytesRead() { return this._bytesRead; }
        get bytesWritten() { return this._bytesWritten; }
    }

    /**
     * Represents a TCP server.
     * @extends EventEmitter
     *
     * Events:
     * - 'listening' - Server started listening
     * - 'connection' - New connection (receives Socket)
     * - 'close' - Server closed
     * - 'error' - Error occurred
     */
    class Server extends EventEmitter {
        constructor(options, connectionListener) {
            super();

            if (typeof options === 'function') {
                connectionListener = options;
                options = {};
            }

            this[kHandle] = null;
            this._connections = new Map();
            this._listening = false;
            this._address = null;

            if (connectionListener) {
                this.on('connection', connectionListener);
            }
        }

        /**
         * Start listening for connections.
         * @param {Object|number} options - Port or options object
         * @param {string} [host='0.0.0.0'] - Host to bind to
         * @param {number} [backlog] - Connection backlog (ignored)
         * @param {Function} [callback] - 'listening' event listener
         * @returns {Server} this
         */
        listen(options, host, backlog, callback) {
            // Normalize arguments
            if (typeof options === 'number') {
                options = { port: options };
            }

            if (typeof host === 'function') {
                callback = host;
                host = undefined;
            } else if (typeof backlog === 'function') {
                callback = backlog;
                backlog = undefined;
            }

            const port = options.port || 0;
            host = options.host || host || '0.0.0.0';

            if (callback) {
                this.once('listening', callback);
            }

            // Call native create_server
            net_create_server(port, host).then(serverId => {
                this[kHandle] = serverId;
                this._listening = true;
                this._updateAddress();
                this.emit('listening');
            }).catch(err => {
                this.emit('error', new Error(err.message || String(err)));
            });

            return this;
        }

        /**
         * Close the server.
         * @param {Function} [callback] - 'close' event listener
         * @returns {Server} this
         */
        close(callback) {
            if (callback) {
                this.once('close', callback);
            }

            if (this[kHandle]) {
                try {
                    net_server_close(this[kHandle]);
                } catch (e) {
                    // Ignore
                }
                this[kHandle] = null;
            }

            this._listening = false;
            return this;
        }

        /**
         * Get the server's address.
         * @returns {{port: number, family: string, address: string}|null}
         */
        address() {
            return this._address;
        }

        _updateAddress() {
            if (!this[kHandle]) return;
            try {
                const info = net_server_address(this[kHandle]);
                this._address = {
                    port: info.port,
                    family: info.address.includes(':') ? 'IPv6' : 'IPv4',
                    address: info.address
                };
            } catch (e) {
                // Ignore
            }
        }

        /**
         * Get the number of concurrent connections.
         * @param {Function} callback - Receives (err, count)
         */
        getConnections(callback) {
            callback(null, this._connections.size);
        }

        get listening() {
            return this._listening;
        }
    }

    // Socket registry for event routing
    const socketRegistry = new Map();
    const serverRegistry = new Map();

    /**
     * Create a new TCP server.
     * @param {Object} [options] - Server options
     * @param {Function} [connectionListener] - 'connection' event listener
     * @returns {Server}
     */
    function createServer(options, connectionListener) {
        const server = new Server(options, connectionListener);

        // Register for event routing
        server.on('listening', () => {
            if (server[kHandle]) {
                serverRegistry.set(server[kHandle], server);
            }
        });

        return server;
    }

    /**
     * Create a new connection to a TCP server.
     * @param {Object|number} options - Port or connection options
     * @param {string} [host] - Host to connect to
     * @param {Function} [callback] - 'connect' event listener
     * @returns {Socket}
     */
    function createConnection(options, host, callback) {
        const socket = new Socket();
        return socket.connect(options, host, callback);
    }

    // Alias
    const connect = createConnection;

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

    /**
     * Handle events from native side.
     * This is called by the runtime when net events occur.
     */
    function handleNetEvent(event) {
        switch (event.type) {
            case 'connection': {
                const server = serverRegistry.get(event.serverId);
                if (server) {
                    const socket = new Socket();
                    socket[kHandle] = event.socketId;
                    socket[kServer] = server;
                    socket._remoteAddress = event.remoteAddress;
                    socket._remotePort = event.remotePort;
                    socket._updateAddressInfo();

                    server._connections.set(event.socketId, socket);
                    socketRegistry.set(event.socketId, socket);

                    server.emit('connection', socket);
                }
                break;
            }
            case 'socketData': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    // Data is base64 encoded
                    const binaryString = atob(event.data);
                    const bytes = new Uint8Array(binaryString.length);
                    for (let i = 0; i < binaryString.length; i++) {
                        bytes[i] = binaryString.charCodeAt(i);
                    }
                    socket._bytesRead += bytes.length;
                    socket.emit('data', bytes);
                }
                break;
            }
            case 'socketEnd': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    socket.readable = false;
                    socket.emit('end');
                }
                break;
            }
            case 'socketClose': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    socketRegistry.delete(event.socketId);
                    if (socket[kServer]) {
                        socket[kServer]._connections.delete(event.socketId);
                    }
                    socket.destroyed = true;
                    socket.readable = false;
                    socket.writable = false;
                    socket.emit('close', event.hadError);
                }
                break;
            }
            case 'socketError': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    socket.emit('error', new Error(event.error));
                }
                break;
            }
            case 'socketConnect': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    socket._updateAddressInfo();
                    socket.emit('connect');
                }
                break;
            }
            case 'socketDrain': {
                const socket = socketRegistry.get(event.socketId);
                if (socket) {
                    socket.emit('drain');
                }
                break;
            }
            case 'serverClose': {
                const server = serverRegistry.get(event.serverId);
                if (server) {
                    serverRegistry.delete(event.serverId);
                    server._listening = false;
                    server.emit('close');
                }
                break;
            }
            case 'serverError': {
                const server = serverRegistry.get(event.serverId);
                if (server) {
                    server.emit('error', new Error(event.error));
                }
                break;
            }
        }
    }

    // Net module
    const netModule = {
        Socket,
        Server,
        createServer,
        createConnection,
        connect,
        isIP,
        isIPv4,
        isIPv6,
        // Internal: event handler for native events
        __handleNetEvent: handleNetEvent,
    };

    // Add default export
    netModule.default = netModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('net', netModule);
    }

    // Register global dispatch function for native events
    // The runtime calls __otter_net_dispatch(jsonString) to deliver events
    globalThis.__otter_net_dispatch = (eventJson) => {
        try {
            const event = JSON.parse(eventJson);
            handleNetEvent(event);
        } catch (e) {
            console.error('Error handling net event:', e);
        }
    };
})();
