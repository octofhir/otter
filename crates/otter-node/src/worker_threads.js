// worker_threads module - Node.js compatible worker threads API

(function() {
    'use strict';

    const EventEmitter = globalThis.__EventEmitter || class EventEmitter {
        constructor() { this._events = new Map(); }
        on(event, listener) {
            if (!this._events.has(event)) this._events.set(event, []);
            this._events.get(event).push(listener);
            return this;
        }
        once(event, listener) {
            const onceWrapper = (...args) => {
                this.off(event, onceWrapper);
                listener.apply(this, args);
            };
            onceWrapper.listener = listener;
            return this.on(event, onceWrapper);
        }
        off(event, listener) {
            if (!this._events.has(event)) return this;
            const listeners = this._events.get(event);
            const idx = listeners.findIndex(l => l === listener || l.listener === listener);
            if (idx !== -1) listeners.splice(idx, 1);
            return this;
        }
        removeListener(event, listener) { return this.off(event, listener); }
        emit(event, ...args) {
            const listeners = this._events.get(event) || [];
            listeners.slice().forEach(fn => fn.apply(this, args));
            return listeners.length > 0;
        }
        removeAllListeners(event) {
            if (event) this._events.delete(event);
            else this._events.clear();
            return this;
        }
    };

    // Track all workers and ports for event polling
    const workers = new Map();
    const ports = new Map();
    const broadcastChannels = new Map();

    // SHARE_ENV symbol for sharing parent's environment
    const SHARE_ENV = Symbol.for('nodejs.worker_threads.SHARE_ENV');

    // Check if we're in the main thread (true by default in main context)
    const isMainThread = __workerThreadsIsMainThread();

    // Thread ID (0 for main thread)
    const threadId = __workerThreadsThreadId();

    // Worker data (null in main thread, set in workers)
    let workerData = null;

    // Parent port (null in main thread, set in workers)
    let parentPort = null;

    // Resource limits for current thread
    const resourceLimits = __workerThreadsGetResourceLimits();

    // ========== MessagePort Class ==========

    class MessagePort extends EventEmitter {
        constructor(portId) {
            super();
            this._id = portId;
            this._started = false;
            this._closed = false;

            // Event handlers (EventTarget-style)
            this.onmessage = null;
            this.onmessageerror = null;
            this.onclose = null;

            ports.set(portId, this);
        }

        postMessage(value, transferList) {
            if (this._closed) {
                throw new Error('Cannot post message on a closed port');
            }
            __messagePortPostMessage(this._id, value, transferList);
        }

        start() {
            if (!this._started && !this._closed) {
                this._started = true;
                __messagePortStart(this._id);
            }
        }

        close() {
            if (!this._closed) {
                this._closed = true;
                __messagePortClose(this._id);
                ports.delete(this._id);
            }
        }

        ref() {
            __messagePortRef(this._id);
            return this;
        }

        unref() {
            __messagePortUnref(this._id);
            return this;
        }

        hasRef() {
            return __messagePortHasRef(this._id);
        }

        // Internal: handle events
        _handleEvent(event) {
            switch (event.type) {
                case 'portMessage':
                    const msgEvent = { data: event.data, target: this };
                    if (this.onmessage) this.onmessage(msgEvent);
                    this.emit('message', event.data);
                    break;
                case 'portMessageError':
                    const errEvent = { error: new Error(event.error), target: this };
                    if (this.onmessageerror) this.onmessageerror(errEvent);
                    this.emit('messageerror', new Error(event.error));
                    break;
                case 'portClose':
                    this._closed = true;
                    if (this.onclose) this.onclose({ target: this });
                    this.emit('close');
                    ports.delete(this._id);
                    break;
            }
        }
    }

    // ========== MessageChannel Class ==========

    class MessageChannel {
        constructor() {
            const result = __messageChannelCreate();
            this.port1 = new MessagePort(result.port1Id);
            this.port2 = new MessagePort(result.port2Id);
        }
    }

    // ========== BroadcastChannel Class ==========

    class BroadcastChannel extends EventEmitter {
        constructor(name) {
            super();
            if (typeof name !== 'string') {
                throw new TypeError('BroadcastChannel name must be a string');
            }
            this._name = name;
            this._id = __broadcastChannelCreate(name);
            this._closed = false;

            // Event handlers
            this.onmessage = null;
            this.onmessageerror = null;

            broadcastChannels.set(this._id, this);
        }

        get name() {
            return this._name;
        }

        postMessage(message) {
            if (this._closed) {
                throw new Error('BroadcastChannel is closed');
            }
            __broadcastChannelPostMessage(this._id, message);
        }

        close() {
            if (!this._closed) {
                this._closed = true;
                __broadcastChannelClose(this._id);
                broadcastChannels.delete(this._id);
            }
        }

        ref() {
            __broadcastChannelRef(this._id);
            return this;
        }

        unref() {
            __broadcastChannelUnref(this._id);
            return this;
        }

        // Internal: handle events
        _handleEvent(event) {
            switch (event.type) {
                case 'broadcastMessage':
                    const msgEvent = { data: event.data, target: this };
                    if (this.onmessage) this.onmessage(msgEvent);
                    this.emit('message', event.data);
                    break;
                case 'broadcastMessageError':
                    const errEvent = { error: new Error(event.error), target: this };
                    if (this.onmessageerror) this.onmessageerror(errEvent);
                    this.emit('messageerror', new Error(event.error));
                    break;
            }
        }
    }

    // ========== Worker Class ==========

    class Worker extends EventEmitter {
        constructor(filename, options = {}) {
            super();

            if (typeof filename !== 'string' && !(filename instanceof URL)) {
                throw new TypeError('filename must be a string or URL');
            }

            const filenameStr = filename instanceof URL ? filename.href : filename;

            // Handle options
            const opts = {
                filename: filenameStr,
                workerData: options.workerData,
                eval: options.eval || false,
                env: options.env === SHARE_ENV ? null : options.env,
                name: options.name,
                resourceLimits: options.resourceLimits,
                argv: options.argv,
                execArgv: options.execArgv,
                stdin: options.stdin || false,
                stdout: options.stdout || false,
                stderr: options.stderr || false,
            };

            this._id = __workerThreadsCreate(opts);
            this._threadId = this._id; // In our implementation, worker ID equals thread ID
            this._exited = false;
            this._exitCode = null;

            // Stdin/stdout/stderr streams (if enabled)
            this.stdin = opts.stdin ? createWritableStream() : null;
            this.stdout = opts.stdout ? createReadableStream() : null;
            this.stderr = opts.stderr ? createReadableStream() : null;

            // Resource limits
            this.resourceLimits = options.resourceLimits || {};

            // Performance (stub)
            this.performance = {
                eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 })
            };

            workers.set(this._id, this);
        }

        get threadId() {
            return this._threadId;
        }

        postMessage(value, transferList) {
            if (this._exited) {
                throw new Error('Cannot post message to terminated worker');
            }
            __workerThreadsPostMessage(this._id, value, transferList);
        }

        terminate() {
            return new Promise((resolve) => {
                if (this._exited) {
                    resolve(this._exitCode);
                    return;
                }

                const onExit = (code) => {
                    this.off('exit', onExit);
                    resolve(code);
                };
                this.on('exit', onExit);

                __workerThreadsTerminate(this._id);
            });
        }

        ref() {
            __workerThreadsRef(this._id);
            return this;
        }

        unref() {
            __workerThreadsUnref(this._id);
            return this;
        }

        // Compatibility: getHeapSnapshot (stub)
        getHeapSnapshot() {
            return Promise.reject(new Error('getHeapSnapshot is not implemented'));
        }

        // Internal: handle events
        _handleEvent(event) {
            switch (event.type) {
                case 'online':
                    this.emit('online');
                    break;
                case 'message':
                    this.emit('message', event.data);
                    break;
                case 'messageerror':
                    this.emit('messageerror', new Error(event.error));
                    break;
                case 'error':
                    this.emit('error', new Error(event.error));
                    break;
                case 'exit':
                    this._exited = true;
                    this._exitCode = event.code;
                    this.emit('exit', event.code);
                    workers.delete(this._id);
                    break;
            }
        }
    }

    // ========== Module Functions ==========

    function getEnvironmentData(key) {
        return __workerThreadsGetEnvData(String(key));
    }

    function setEnvironmentData(key, value) {
        __workerThreadsSetEnvData(String(key), value);
    }

    function receiveMessageOnPort(port) {
        if (!(port instanceof MessagePort)) {
            throw new TypeError('port must be a MessagePort');
        }
        const result = __receiveMessageOnPort(port._id);
        return result ? { message: result.message } : undefined;
    }

    // Track untransferable objects using a WeakMap
    const untransferableMap = new WeakMap();

    function markAsUntransferable(object) {
        if (object === null || typeof object !== 'object') {
            throw new TypeError('markAsUntransferable expects an object');
        }
        const id = __markAsUntransferable();
        untransferableMap.set(object, id);
    }

    function isMarkedAsUntransferable(object) {
        if (object === null || typeof object !== 'object') {
            return false;
        }
        const id = untransferableMap.get(object);
        if (id === undefined) return false;
        return __isMarkedAsUntransferable(id);
    }

    // Move message port to a different context (stub - just returns a reference)
    function moveMessagePortToContext(port, context) {
        // In Node.js, this is used with vm.Context
        // We provide a stub that just returns the port
        return port;
    }

    // ========== Helper Functions ==========

    function createReadableStream() {
        // Stub for stdout/stderr streams
        return {
            on: function() { return this; },
            once: function() { return this; },
            pipe: function(dest) { return dest; },
            read: function() { return null; },
            setEncoding: function() { return this; },
        };
    }

    function createWritableStream() {
        // Stub for stdin stream
        return {
            on: function() { return this; },
            once: function() { return this; },
            write: function() { return true; },
            end: function() {},
            setDefaultEncoding: function() { return this; },
        };
    }

    // ========== Event Loop Polling ==========

    function pollWorkerThreadsEvents() {
        const events = __workerThreadsPollEvents();

        for (const event of events) {
            // Route events to appropriate handlers
            if (event.workerId !== undefined) {
                // Worker events
                const worker = workers.get(event.workerId);
                if (worker) {
                    worker._handleEvent(event);
                }
            } else if (event.portId !== undefined) {
                // MessagePort events
                const port = ports.get(event.portId);
                if (port) {
                    port._handleEvent(event);
                }
            } else if (event.channelId !== undefined) {
                // BroadcastChannel events
                const channel = broadcastChannels.get(event.channelId);
                if (channel) {
                    channel._handleEvent(event);
                }
            }
        }

        return events.length;
    }

    // Register poll handler with the global poll system
    if (typeof globalThis.__otter_register_poll_handler === 'function') {
        globalThis.__otter_register_poll_handler(pollWorkerThreadsEvents);
    }

    // Also expose for direct access
    globalThis.__otter_worker_threads_poll = pollWorkerThreadsEvents;

    // ========== Module Exports ==========

    const workerThreadsModule = {
        // Properties
        isMainThread,
        parentPort,
        workerData,
        threadId,
        resourceLimits,
        SHARE_ENV,

        // Functions
        getEnvironmentData,
        setEnvironmentData,
        receiveMessageOnPort,
        markAsUntransferable,
        isMarkedAsUntransferable,
        moveMessagePortToContext,

        // Classes
        Worker,
        MessageChannel,
        MessagePort,
        BroadcastChannel,
    };

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('worker_threads', workerThreadsModule);
    }

    // Also export to globalThis for debugging
    globalThis.__workerThreadsModule = workerThreadsModule;
})();
