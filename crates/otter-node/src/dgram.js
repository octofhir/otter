/**
 * node:dgram - UDP/datagram sockets module.
 *
 * Provides Node.js-compatible UDP socket API.
 *
 * @example
 * const dgram = require('dgram');
 *
 * // Create a UDP server
 * const server = dgram.createSocket('udp4');
 * server.on('message', (msg, rinfo) => {
 *     console.log(`server got: ${msg} from ${rinfo.address}:${rinfo.port}`);
 * });
 * server.bind(41234);
 *
 * // Create a UDP client
 * const client = dgram.createSocket('udp4');
 * client.send('Hello', 41234, 'localhost');
 */
(function() {
    'use strict';

    const EventEmitter = globalThis.EventEmitter;

    /**
     * Socket class representing a UDP socket.
     */
    class Socket extends EventEmitter {
        constructor(type, callback) {
            super();

            this._type = type === 'udp6' ? 'udp6' : 'udp4';
            this._socketId = null;
            this._bound = false;
            this._closed = false;
            this._address = null;

            if (typeof callback === 'function') {
                this.on('message', callback);
            }
        }

        /**
         * Bind the socket to an address and port.
         * @param {number|Object} port - Port number or options object
         * @param {string} [address] - Address to bind to
         * @param {Function} [callback] - Callback when bound
         */
        bind(port, address, callback) {
            if (this._closed) {
                const err = new Error('Socket is closed');
                err.code = 'ERR_SOCKET_CLOSED';
                throw err;
            }

            // Handle different argument signatures
            let options = {};
            if (typeof port === 'object') {
                options = port;
                callback = address;
                port = options.port || 0;
                address = options.address;
            }

            if (typeof address === 'function') {
                callback = address;
                address = undefined;
            }

            port = port || 0;
            address = address || (this._type === 'udp6' ? '::' : '0.0.0.0');

            if (callback) {
                this.once('listening', callback);
            }

            // Create socket and bind
            this._createAndBind(port, address);

            return this;
        }

        async _createAndBind(port, address) {
            try {
                // Create socket
                const createResult = await __otter_dgram_create_socket(this._type);
                this._socketId = createResult.socketId;

                // Bind socket
                const bindResult = await __otter_dgram_bind(this._socketId, port, address);
                this._bound = true;
                this._address = {
                    address: bindResult.address,
                    port: bindResult.port,
                    family: bindResult.family
                };

                this.emit('listening');
            } catch (err) {
                this.emit('error', err);
            }
        }

        /**
         * Send data to a remote address.
         * @param {Buffer|string|Array} msg - Message to send
         * @param {number} [offset] - Offset in the buffer
         * @param {number} [length] - Number of bytes to send
         * @param {number} port - Destination port
         * @param {string} [address] - Destination address
         * @param {Function} [callback] - Callback when sent
         */
        send(msg, offset, length, port, address, callback) {
            if (this._closed) {
                const err = new Error('Socket is closed');
                err.code = 'ERR_SOCKET_CLOSED';
                if (callback) {
                    process.nextTick(() => callback(err));
                }
                return;
            }

            // Handle different argument signatures
            // send(msg, port, address, callback)
            if (typeof offset === 'number' && typeof length !== 'number') {
                callback = address;
                address = port;
                port = offset;
                offset = 0;
                length = msg.length;
            }
            // send(msg, port, callback)
            else if (typeof offset === 'number' && typeof length === 'string') {
                callback = port;
                address = length;
                port = offset;
                offset = 0;
                length = msg.length;
            }

            // Default address
            if (!address) {
                address = this._type === 'udp6' ? '::1' : '127.0.0.1';
            }

            this._doSend(msg, offset, length, port, address, callback);
        }

        async _doSend(msg, offset, length, port, address, callback) {
            try {
                // Ensure socket is created (for unbound send)
                if (!this._socketId) {
                    const createResult = await __otter_dgram_create_socket(this._type);
                    this._socketId = createResult.socketId;

                    // Bind to ephemeral port for sending
                    const bindResult = await __otter_dgram_bind(this._socketId, 0, '0.0.0.0');
                    this._bound = true;
                    this._address = {
                        address: bindResult.address,
                        port: bindResult.port,
                        family: bindResult.family
                    };
                }

                // Convert message to base64
                let data;
                if (typeof msg === 'string') {
                    data = btoa(msg);
                } else if (msg instanceof Uint8Array || Buffer.isBuffer(msg)) {
                    // Slice if offset/length provided
                    const slice = msg.slice(offset || 0, (offset || 0) + (length || msg.length));
                    data = btoa(String.fromCharCode.apply(null, slice));
                } else if (Array.isArray(msg)) {
                    data = btoa(String.fromCharCode.apply(null, msg));
                } else {
                    throw new Error('Invalid message type');
                }

                const result = await __otter_dgram_send(this._socketId, data, port, address);

                if (callback) {
                    callback(null, result.bytesSent);
                }
            } catch (err) {
                if (callback) {
                    callback(err);
                } else {
                    this.emit('error', err);
                }
            }
        }

        /**
         * Close the socket.
         * @param {Function} [callback] - Callback when closed
         */
        close(callback) {
            if (this._closed) {
                return this;
            }

            if (callback) {
                this.once('close', callback);
            }

            this._closed = true;

            if (this._socketId) {
                __otter_dgram_close(this._socketId)
                    .then(() => {
                        this.emit('close');
                    })
                    .catch(err => {
                        this.emit('error', err);
                    });
            } else {
                process.nextTick(() => this.emit('close'));
            }

            return this;
        }

        /**
         * Get the address information for the socket.
         * @returns {Object} Address information
         */
        address() {
            if (!this._bound || !this._address) {
                throw new Error('Socket is not bound');
            }
            return this._address;
        }

        /**
         * Set the TTL (Time To Live) for outgoing packets.
         * @param {number} ttl - TTL value (1-255)
         */
        setTTL(ttl) {
            // Not implemented yet - would require socket options
            return this;
        }

        /**
         * Set multicast TTL.
         * @param {number} ttl - TTL value
         */
        setMulticastTTL(ttl) {
            // Not implemented yet
            return this;
        }

        /**
         * Set multicast loopback.
         * @param {boolean} flag - Enable/disable loopback
         */
        setMulticastLoopback(flag) {
            // Not implemented yet
            return this;
        }

        /**
         * Set broadcast flag.
         * @param {boolean} flag - Enable/disable broadcast
         */
        setBroadcast(flag) {
            // Not implemented yet
            return this;
        }

        /**
         * Add membership to a multicast group.
         * @param {string} multicastAddress - Multicast group address
         * @param {string} [multicastInterface] - Interface to use
         */
        addMembership(multicastAddress, multicastInterface) {
            // Not implemented yet
            return this;
        }

        /**
         * Drop membership from a multicast group.
         * @param {string} multicastAddress - Multicast group address
         * @param {string} [multicastInterface] - Interface
         */
        dropMembership(multicastAddress, multicastInterface) {
            // Not implemented yet
            return this;
        }

        /**
         * Set the multicast interface.
         * @param {string} multicastInterface - Interface address
         */
        setMulticastInterface(multicastInterface) {
            // Not implemented yet
            return this;
        }

        /**
         * Ref the socket (keep event loop alive).
         */
        ref() {
            // No-op for now
            return this;
        }

        /**
         * Unref the socket (allow event loop to exit).
         */
        unref() {
            // No-op for now
            return this;
        }

        /**
         * Get receive buffer size.
         * @returns {number} Buffer size
         */
        getRecvBufferSize() {
            // Default buffer size
            return 65536;
        }

        /**
         * Get send buffer size.
         * @returns {number} Buffer size
         */
        getSendBufferSize() {
            // Default buffer size
            return 65536;
        }

        /**
         * Set receive buffer size.
         * @param {number} size - Buffer size
         */
        setRecvBufferSize(size) {
            // Not implemented yet
            return this;
        }

        /**
         * Set send buffer size.
         * @param {number} size - Buffer size
         */
        setSendBufferSize(size) {
            // Not implemented yet
            return this;
        }

        /**
         * Get remote address info.
         * @returns {Object} Remote address info
         */
        remoteAddress() {
            // UDP is connectionless, this would require connect()
            throw new Error('Not connected');
        }

        /**
         * Connect to a remote address (make socket a connected socket).
         * @param {number} port - Remote port
         * @param {string} [address] - Remote address
         * @param {Function} [callback] - Callback when connected
         */
        connect(port, address, callback) {
            // Not implemented yet - would require native support
            if (typeof address === 'function') {
                callback = address;
                address = undefined;
            }
            if (callback) {
                process.nextTick(() => callback(new Error('connect() not implemented')));
            }
            return this;
        }

        /**
         * Disconnect a connected socket.
         */
        disconnect() {
            // Not implemented yet
            return this;
        }
    }

    /**
     * Create a UDP socket.
     * @param {string|Object} type - Socket type ('udp4' or 'udp6') or options
     * @param {Function} [callback] - Message callback
     * @returns {Socket} The created socket
     */
    function createSocket(type, callback) {
        let options = {};

        if (typeof type === 'object') {
            options = type;
            type = options.type;
            callback = callback || options.lookup;
        }

        if (type !== 'udp4' && type !== 'udp6') {
            throw new Error(`Bad socket type specified. Valid types are: udp4, udp6`);
        }

        return new Socket(type, callback);
    }

    // Export the dgram module
    const dgram = {
        Socket,
        createSocket
    };

    // Register the module
    if (typeof __registerModule === 'function') {
        __registerModule('dgram', dgram);
        __registerModule('node:dgram', dgram);
    }

    // Also make available globally for direct use
    globalThis.dgram = dgram;
})();
