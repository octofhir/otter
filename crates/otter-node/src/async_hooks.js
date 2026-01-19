/**
 * node:async_hooks - Node.js Async Hooks module (stub implementation)
 *
 * Phase 1 stub providing the API surface for Express dependencies
 * (raw-body, on-finished) which gracefully degrade when available.
 *
 * @see https://nodejs.org/api/async_hooks.html
 */
(function() {
    'use strict';

    const getExecutionAsyncId = () => __otter_async_hooks_execution_async_id();
    const getTriggerAsyncId = () => __otter_async_hooks_trigger_async_id();
    const nextAsyncId = () => __otter_async_hooks_next_async_id();
    const setCurrentAsyncIds = (asyncId, triggerAsyncId) =>
        __otter_async_hooks_set_current(asyncId, triggerAsyncId);

    let currentAsyncResource = null;
    const activeHooks = new Set();
    const asyncLocalStorages = new Set();

    function emitHookEvent(name, asyncId, type, triggerAsyncId, resource) {
        for (const hook of activeHooks) {
            const callback = hook._callbacks && hook._callbacks[name];
            if (typeof callback === 'function') {
                callback(asyncId, type, triggerAsyncId, resource);
            }
        }
    }

    /**
     * Returns the asyncId of the current execution context.
     * @returns {number}
     */
    function executionAsyncId() {
        return getExecutionAsyncId();
    }

    /**
     * Returns the ID of the resource responsible for calling
     * the callback that is currently being executed.
     * @returns {number}
     */
    function triggerAsyncId() {
        return getTriggerAsyncId();
    }

    /**
     * Returns the current execution async resource.
     * @returns {*}
     */
    function executionAsyncResource() {
        return currentAsyncResource;
    }

    /**
     * AsyncResource class - represents an async resource.
     * Used by libraries to manually track async contexts.
     */
    class AsyncResource {
        /**
         * @param {string} type - The type of async resource
         * @param {number|object} [triggerAsyncIdOrOptions] - Trigger ID or options object
         */
        constructor(type, triggerAsyncIdOrOptions) {
            if (typeof type !== 'string') {
                throw new TypeError('The "type" argument must be of type string');
            }

            this._type = type;
            this._asyncId = nextAsyncId();
            this._destroyed = false;

            // Handle both forms: number or { triggerAsyncId, requireManualDestroy }
            if (typeof triggerAsyncIdOrOptions === 'number') {
                this._triggerAsyncId = triggerAsyncIdOrOptions;
            } else if (triggerAsyncIdOrOptions && typeof triggerAsyncIdOrOptions === 'object') {
                this._triggerAsyncId = triggerAsyncIdOrOptions.triggerAsyncId ?? executionAsyncId();
            } else {
                this._triggerAsyncId = executionAsyncId();
            }

            emitHookEvent('init', this._asyncId, this._type, this._triggerAsyncId, this);
        }

        /**
         * Returns the unique ID assigned to the AsyncResource instance.
         * @returns {number}
         */
        asyncId() {
            return this._asyncId;
        }

        /**
         * Returns the trigger ID for the AsyncResource instance.
         * @returns {number}
         */
        triggerAsyncId() {
            return this._triggerAsyncId;
        }

        /**
         * Call the provided function within the async context of this resource.
         * @param {Function} fn - Function to call
         * @param {*} thisArg - this argument for fn
         * @param {...*} args - Arguments to pass to fn
         * @returns {*} Return value of fn
         */
        runInAsyncScope(fn, thisArg, ...args) {
            const previousIds = setCurrentAsyncIds(this._asyncId, this._triggerAsyncId);
            const previousResource = currentAsyncResource;
            currentAsyncResource = this;
            emitHookEvent('before', this._asyncId, this._type, this._triggerAsyncId, this);

            try {
                return fn.apply(thisArg, args);
            } finally {
                emitHookEvent('after', this._asyncId, this._type, this._triggerAsyncId, this);
                currentAsyncResource = previousResource;
                setCurrentAsyncIds(previousIds.async_id, previousIds.trigger_async_id);
            }
        }

        /**
         * Manually emit the before hook for this resource.
         */
        emitBefore() {
            emitHookEvent('before', this._asyncId, this._type, this._triggerAsyncId, this);
        }

        /**
         * Manually emit the after hook for this resource.
         */
        emitAfter() {
            emitHookEvent('after', this._asyncId, this._type, this._triggerAsyncId, this);
        }

        /**
         * Call AsyncHooks destroy callbacks.
         * @returns {AsyncResource} this
         */
        emitDestroy() {
            if (!this._destroyed) {
                this._destroyed = true;
                emitHookEvent('destroy', this._asyncId, this._type, this._triggerAsyncId, this);
            }
            return this;
        }

        /**
         * Binds the given function to execute in this resource's async context.
         * @param {Function} fn - Function to bind
         * @param {*} [thisArg] - this argument for fn
         * @returns {Function} Bound function
         */
        bind(fn, thisArg) {
            if (typeof fn !== 'function') {
                throw new TypeError('The "fn" argument must be of type function');
            }
            const resource = this;
            const bound = function(...args) {
                return resource.runInAsyncScope(fn, thisArg !== undefined ? thisArg : this, ...args);
            };
            Object.defineProperty(bound, 'length', {
                configurable: true,
                enumerable: false,
                value: fn.length,
                writable: false,
            });
            return bound;
        }

        /**
         * Static method to bind a function to the current execution context.
         * @param {Function} fn - Function to bind
         * @param {string} [type] - Resource type name
         * @param {*} [thisArg] - this argument for fn
         * @returns {Function} Bound function
         */
        static bind(fn, type, thisArg) {
            type = type || fn.name || 'bound-anonymous-fn';
            return new AsyncResource(type).bind(fn, thisArg);
        }
    }

    /**
     * AsyncLocalStorage - provides async context propagation.
     * Similar to thread-local storage but for async contexts.
     */
    class AsyncLocalStorage {
        #store = undefined;
        #enabled = true;

        /**
         * @param {object} [options] - Options object
         */
        constructor(options) {
            // Node.js 19+ added options.onPropagate
            if (options && typeof options === 'object') {
                this._onPropagate = options.onPropagate;
            }
            asyncLocalStorages.add(this);
        }

        /**
         * Disables the AsyncLocalStorage instance.
         */
        disable() {
            this.#enabled = false;
            this.#store = undefined;
        }

        /**
         * Returns the current store.
         * @returns {*} Current store value or undefined
         */
        getStore() {
            if (!this.#enabled) {
                return undefined;
            }
            return this.#store;
        }

        /**
         * Runs a function synchronously within a context.
         * @param {*} store - Store value for this context
         * @param {Function} callback - Function to run
         * @param {...*} args - Arguments for callback
         * @returns {*} Return value of callback
         */
        run(store, callback, ...args) {
            if (!this.#enabled) {
                return callback(...args);
            }

            const previousStore = this.#store;
            this.#store = store;

            try {
                return callback(...args);
            } finally {
                this.#store = previousStore;
            }
        }

        /**
         * Exits the current context and runs callback with no store.
         * @param {Function} callback - Function to run
         * @param {...*} args - Arguments for callback
         * @returns {*} Return value of callback
         */
        exit(callback, ...args) {
            if (!this.#enabled) {
                return callback(...args);
            }

            const previousStore = this.#store;
            this.#store = undefined;

            try {
                return callback(...args);
            } finally {
                this.#store = previousStore;
            }
        }

        /**
         * Transitions into a context for the remainder of the current
         * synchronous execution.
         * @param {*} store - Store value
         */
        enterWith(store) {
            if (this.#enabled) {
                this.#store = store;
            }
        }

        /**
         * Static method to bind a function to the current storage context.
         * @param {Function} fn - Function to bind
         * @returns {Function} Bound function
         */
        static bind(fn) {
            if (typeof fn !== 'function') {
                throw new TypeError('The "fn" argument must be of type function');
            }
            const snapshot = [];
            for (const storage of asyncLocalStorages) {
                snapshot.push({
                    storage,
                    enabled: storage.#enabled,
                    store: storage.#store,
                });
            }
            return function(...args) {
                const previous = [];
                for (const entry of snapshot) {
                    const storage = entry.storage;
                    previous.push({
                        storage,
                        enabled: storage.#enabled,
                        store: storage.#store,
                    });
                    if (entry.enabled && storage.#enabled) {
                        storage.#store = entry.store;
                    }
                }
                try {
                    return fn.apply(this, args);
                } finally {
                    for (const entry of previous) {
                        entry.storage.#enabled = entry.enabled;
                        entry.storage.#store = entry.store;
                    }
                }
            };
        }

        /**
         * Static method to capture the current execution context.
         * @returns {Function} Snapshot function
         */
        static snapshot() {
            const snapshot = [];
            for (const storage of asyncLocalStorages) {
                snapshot.push({
                    storage,
                    enabled: storage.#enabled,
                    store: storage.#store,
                });
            }
            return function(fn, ...args) {
                if (typeof fn !== 'function') {
                    throw new TypeError('The "fn" argument must be of type function');
                }
                const previous = [];
                for (const entry of snapshot) {
                    const storage = entry.storage;
                    previous.push({
                        storage,
                        enabled: storage.#enabled,
                        store: storage.#store,
                    });
                    if (entry.enabled && storage.#enabled) {
                        storage.#store = entry.store;
                    }
                }
                try {
                    return fn(...args);
                } finally {
                    for (const entry of previous) {
                        entry.storage.#enabled = entry.enabled;
                        entry.storage.#store = entry.store;
                    }
                }
            };
        }
    }

    /**
     * Creates an async hook (stub implementation).
     * Used by profilers and debugging tools.
     * @param {object} callbacks - Hook callbacks
     * @returns {object} Hook object with enable/disable
     */
    function createHook(callbacks) {
        const hook = {
            _callbacks: callbacks || {},
            _enabled: false,
            enable() {
                if (!this._enabled) {
                    this._enabled = true;
                    activeHooks.add(this);
                }
                return this;
            },
            disable() {
                if (this._enabled) {
                    this._enabled = false;
                    activeHooks.delete(this);
                }
                return this;
            },
        };
        return hook;
    }

    // Module exports
    const asyncHooksModule = {
        // Functions
        executionAsyncId,
        triggerAsyncId,
        executionAsyncResource,
        createHook,

        // Classes
        AsyncResource,
        AsyncLocalStorage,

        // Deprecated aliases (Node.js compatibility)
        currentId: executionAsyncId,
        triggerId: triggerAsyncId,
    };

    // Default export for ESM
    asyncHooksModule.default = asyncHooksModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('async_hooks', asyncHooksModule);
    }
})();
