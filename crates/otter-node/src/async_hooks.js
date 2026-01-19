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

    // Simple incrementing ID counter for async contexts
    let asyncIdCounter = 1;

    // Current execution context tracking
    let currentAsyncId = 1;      // Root async ID
    let currentTriggerAsyncId = 0;

    /**
     * Returns the asyncId of the current execution context.
     * @returns {number}
     */
    function executionAsyncId() {
        return currentAsyncId;
    }

    /**
     * Returns the ID of the resource responsible for calling
     * the callback that is currently being executed.
     * @returns {number}
     */
    function triggerAsyncId() {
        return currentTriggerAsyncId;
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
            this._asyncId = ++asyncIdCounter;
            this._destroyed = false;

            // Handle both forms: number or { triggerAsyncId, requireManualDestroy }
            if (typeof triggerAsyncIdOrOptions === 'number') {
                this._triggerAsyncId = triggerAsyncIdOrOptions;
            } else if (triggerAsyncIdOrOptions && typeof triggerAsyncIdOrOptions === 'object') {
                this._triggerAsyncId = triggerAsyncIdOrOptions.triggerAsyncId ?? executionAsyncId();
            } else {
                this._triggerAsyncId = executionAsyncId();
            }
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
            const prevAsyncId = currentAsyncId;
            const prevTriggerAsyncId = currentTriggerAsyncId;

            try {
                currentAsyncId = this._asyncId;
                currentTriggerAsyncId = this._triggerAsyncId;
                return fn.apply(thisArg, args);
            } finally {
                currentAsyncId = prevAsyncId;
                currentTriggerAsyncId = prevTriggerAsyncId;
            }
        }

        /**
         * Call AsyncHooks destroy callbacks.
         * @returns {AsyncResource} this
         */
        emitDestroy() {
            if (!this._destroyed) {
                this._destroyed = true;
                // In a full implementation, would trigger destroy hooks
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
            // Stub: return function as-is
            // Full implementation would capture current ALS contexts
            return fn;
        }

        /**
         * Static method to capture the current execution context.
         * @returns {Function} Snapshot function
         */
        static snapshot() {
            // Stub: return a pass-through function
            return function(fn, ...args) {
                return fn(...args);
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
        // Stub: return minimal hook object
        return {
            enable() { return this; },
            disable() { return this; },
        };
    }

    // Module exports
    const asyncHooksModule = {
        // Functions
        executionAsyncId,
        triggerAsyncId,
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
