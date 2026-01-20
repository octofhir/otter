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
    const setCurrentAsyncIds = (asyncId, triggerAsyncId) => {
        const previous = __otter_async_hooks_set_current(asyncId, triggerAsyncId);
        updateAsyncLocalStorageContext(asyncId, triggerAsyncId);
        return previous;
    };

    let currentAsyncResource = null;
    const activeHooks = new Set();
    const asyncLocalStorages = new Set();

    function updateAsyncLocalStorageContext(asyncId, triggerAsyncId) {
        for (const storage of asyncLocalStorages) {
            storage._updateContext(asyncId, triggerAsyncId);
        }
    }

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
        #storeMap = new Map();
        #currentStore = undefined;
        #enabled = true;
        #suspended = 0;
        #defaultValue = undefined;
        name = undefined;

        /**
         * @param {object} [options] - Options object
         */
        constructor(options) {
            if (options && typeof options === 'object') {
                this._onPropagate = options.onPropagate;
                if (options.defaultValue !== undefined) {
                    this.#defaultValue = options.defaultValue;
                }
                if (typeof options.name === 'string') {
                    this.name = options.name;
                }
            }
            asyncLocalStorages.add(this);
        }

        /**
         * Disables the AsyncLocalStorage instance.
         */
        disable() {
            this.#enabled = false;
            this.#storeMap.clear();
            this.#currentStore = undefined;
        }

        /**
         * Returns the current store.
         * @returns {*} Current store value or undefined
         */
        getStore() {
            if (!this.#enabled || this.#suspended > 0) {
                return undefined;
            }
            return this.#currentStore;
        }

        /**
         * Runs a function synchronously within a context.
         * @param {*} store - Store value for this context
         * @param {Function} callback - Function to run
         * @param {...*} args - Arguments for callback
         * @returns {*} Return value of callback
         */
        run(store, callback, ...args) {
            if (typeof callback !== 'function') {
                throw new TypeError('The "callback" argument must be of type function');
            }

            if (!this.#enabled) {
                return callback(...args);
            }

            const resource = new AsyncResource('AsyncLocalStorage.run', executionAsyncId());
            this.#storeMap.set(resource._asyncId, store);
            return resource.runInAsyncScope(() => callback(...args));
        }

        /**
         * Exits the current context and runs callback with no store.
         * @param {Function} callback - Function to run
         * @param {...*} args - Arguments for callback
         * @returns {*} Return value of callback
         */
        exit(callback, ...args) {
            if (typeof callback !== 'function') {
                throw new TypeError('The "callback" argument must be of type function');
            }

            if (!this.#enabled) {
                return callback(...args);
            }

            this.#suspended += 1;
            try {
                return callback(...args);
            } finally {
                this.#suspended -= 1;
            }
        }

        /**
         * Transitions into a context for the remainder of the current
         * synchronous execution.
         * @param {*} store - Store value
         */
        enterWith(store) {
            if (!this.#enabled) {
                return;
            }
            const id = executionAsyncId();
            this.#storeMap.set(id, store);
            this._updateContext(id, triggerAsyncId());
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
            const snapshot = AsyncLocalStorage.snapshot();
            return function(...args) {
                return snapshot(fn, ...args);
            };
        }

        /**
         * Static method to capture the current execution context.
         * @returns {Function} Snapshot function
         */
        static snapshot() {
            const saved = [];
            for (const storage of asyncLocalStorages) {
                saved.push({
                    storage,
                    state: storage._captureState(),
                });
            }
            return function(fn, ...args) {
                if (typeof fn !== 'function') {
                    throw new TypeError('The "fn" argument must be of type function');
                }
                const prevStates = [];
                for (const entry of saved) {
                    prevStates.push(entry.storage._captureState());
                    entry.storage._setStoreForCurrentId(entry.state.currentStore);
                }
                try {
                    return fn(...args);
                } finally {
                    for (let i = 0; i < saved.length; i++) {
                        saved[i].storage._restoreState(prevStates[i]);
                    }
                }
            };
        }

        _setStoreForCurrentId(store) {
            if (!this.#enabled) {
                return;
            }
            const id = executionAsyncId();
            this.#storeMap.set(id, store);
            this._updateContext(id, triggerAsyncId());
        }

        _captureState() {
            return {
                enabled: this.#enabled,
                suspended: this.#suspended,
                currentStore: this.#currentStore,
            };
        }

        _restoreState(state) {
            this.#enabled = state.enabled;
            this.#suspended = state.suspended;
            this.#currentStore = state.currentStore;
        }

        _updateContext(asyncId, triggerAsyncId) {
            if (!this.#enabled || this.#suspended > 0) {
                this.#currentStore = undefined;
                return;
            }
            if (this.#storeMap.has(asyncId)) {
                this.#currentStore = this.#storeMap.get(asyncId);
                return;
            }
            if (this.#storeMap.has(triggerAsyncId)) {
                const parent = this.#storeMap.get(triggerAsyncId);
                this.#storeMap.set(asyncId, parent);
                this.#currentStore = parent;
                return;
            }
            if (this.#defaultValue !== undefined) {
                this.#storeMap.set(asyncId, this.#defaultValue);
                this.#currentStore = this.#defaultValue;
                return;
            }
            this.#currentStore = undefined;
        }
    }

    function wrapWithAsyncResource(type, callback) {
        if (typeof callback !== 'function') {
            throw new TypeError('The "callback" argument must be of type function');
        }
        const triggerId = executionAsyncId();
        return function(...args) {
            const resource = new AsyncResource(type, triggerId);
            return resource.runInAsyncScope(() => callback.apply(this, args));
        };
    }

    function wrapGlobalAsyncFn(name, type) {
        const original = globalThis[name];
        if (typeof original !== 'function') {
            return;
        }
        const flag = `__otter_async_hooks_${name}_wrapped`;
        if (globalThis[flag]) {
            return;
        }
        globalThis[flag] = true;
        globalThis[name] = function(callback, ...rest) {
            return original.call(this, wrapWithAsyncResource(type, callback), ...rest);
        };
    }

    function wrapQueueMicrotask() {
        if (typeof globalThis.queueMicrotask !== 'function') {
            return;
        }
        const flag = '__otter_async_hooks_queueMicrotask_wrapped';
        if (globalThis[flag]) {
            return;
        }
        const original = globalThis.queueMicrotask;
        globalThis[flag] = true;
        globalThis.queueMicrotask = function queueMicrotaskPatched(callback) {
            return original.call(globalThis, wrapWithAsyncResource('Microtask', callback));
        };
    }

    wrapGlobalAsyncFn('setTimeout', 'Timeout');
    wrapGlobalAsyncFn('setInterval', 'Interval');
    wrapGlobalAsyncFn('setImmediate', 'Immediate');
    wrapQueueMicrotask();

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
