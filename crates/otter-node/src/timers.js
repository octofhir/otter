// node:timers module implementation
// Provides setTimeout/setInterval/setImmediate wrappers with Node-like handles

(function() {
    function normalizeDelay(delay) {
        const value = Number(delay);
        if (!Number.isFinite(value) || value < 0) {
            return 0;
        }
        return value;
    }

    function createAbortError(reason) {
        if (reason instanceof Error) {
            reason.name = 'AbortError';
            return reason;
        }
        const err = new Error('The operation was aborted');
        err.name = 'AbortError';
        if (reason !== undefined) {
            err.cause = reason;
        }
        return err;
    }

    function addAbortListener(signal, onAbort) {
        if (!signal || typeof signal.addEventListener !== 'function') {
            return () => {};
        }
        signal.addEventListener('abort', onAbort, { once: true });
        return () => signal.removeEventListener('abort', onAbort);
    }

    function applyRef(kind, id, refed) {
        if (id == null) {
            return;
        }
        const fn = kind === 'immediate'
            ? globalThis.__otter_immediate_ref
            : globalThis.__otter_timer_ref;
        if (typeof fn === 'function') {
            fn(id, refed);
        }
    }

    class TimerHandle {
        constructor(kind, callback, delay, args) {
            this._kind = kind;
            this._callback = callback;
            this._delay = delay;
            this._args = args;
            this._id = null;
            this._active = true;
            this._refed = true;
            this._schedule();
        }

        _schedule() {
            const invoke = () => {
                if (!this._active) {
                    return;
                }
                if (this._kind !== 'interval') {
                    this._active = false;
                    this._id = null;
                }
                this._callback(...this._args);
            };

            if (this._kind === 'interval') {
                this._id = globalThis.setInterval(invoke, this._delay);
            } else {
                if (this._kind === 'immediate') {
                    this._id = globalThis.setImmediate(invoke);
                } else {
                    this._id = globalThis.setTimeout(invoke, this._delay);
                }
            }

            applyRef(this._kind, this._id, this._refed);
        }

        _clear() {
            if (this._id == null) {
                return;
            }
            if (this._kind === 'interval') {
                globalThis.clearInterval(this._id);
            } else {
                if (this._kind === 'immediate') {
                    globalThis.clearImmediate(this._id);
                } else {
                    globalThis.clearTimeout(this._id);
                }
            }
            this._id = null;
        }

        ref() {
            this._refed = true;
            applyRef(this._kind, this._id, true);
            return this;
        }

        unref() {
            this._refed = false;
            applyRef(this._kind, this._id, false);
            return this;
        }

        hasRef() {
            return this._refed && this._active;
        }

        refresh() {
            if (!this._active) {
                return this;
            }
            this._clear();
            this._schedule();
            return this;
        }

        [Symbol.toPrimitive]() {
            return this._id == null ? 0 : this._id;
        }

        valueOf() {
            return this._id == null ? 0 : this._id;
        }
    }

    function assertCallback(callback) {
        if (typeof callback !== 'function') {
            throw new TypeError('Callback must be a function');
        }
    }

    function setTimeoutCompat(callback, delay, ...args) {
        assertCallback(callback);
        return new TimerHandle('timeout', callback, normalizeDelay(delay), args);
    }

    function setIntervalCompat(callback, delay, ...args) {
        assertCallback(callback);
        return new TimerHandle('interval', callback, normalizeDelay(delay), args);
    }

    function setImmediateCompat(callback, ...args) {
        assertCallback(callback);
        return new TimerHandle('immediate', callback, 0, args);
    }

    function clearTimer(handle) {
        if (handle instanceof TimerHandle) {
            handle._active = false;
            handle._clear();
            return;
        }
        return handle;
    }

    function clearTimeoutCompat(handle) {
        const value = clearTimer(handle);
        if (value !== handle) {
            return;
        }
        globalThis.clearTimeout(handle);
    }

    function clearIntervalCompat(handle) {
        const value = clearTimer(handle);
        if (value !== handle) {
            return;
        }
        globalThis.clearInterval(handle);
    }

    function clearImmediateCompat(handle) {
        const value = clearTimer(handle);
        if (value !== handle) {
            return;
        }
        globalThis.clearImmediate(handle);
    }

    function resolveRefOption(options) {
        if (!options || typeof options !== 'object') {
            return true;
        }
        if (typeof options.ref === 'boolean') {
            return options.ref;
        }
        return true;
    }

    function promiseDelay(delay, value, options) {
        const ms = normalizeDelay(delay);
        const signal = options && options.signal;
        const refed = resolveRefOption(options);

        return new Promise((resolve, reject) => {
            if (signal && signal.aborted) {
                reject(createAbortError(signal.reason));
                return;
            }

            let id = null;
            let cleanup = () => {};

            const onAbort = () => {
                if (id != null) {
                    globalThis.clearTimeout(id);
                }
                cleanup();
                reject(createAbortError(signal.reason));
            };

            cleanup = addAbortListener(signal, onAbort);

            id = globalThis.setTimeout(() => {
                cleanup();
                resolve(value);
            }, ms);
            applyRef('timeout', id, refed);
        });
    }

    function promiseImmediate(value, options) {
        const signal = options && options.signal;
        const refed = resolveRefOption(options);

        return new Promise((resolve, reject) => {
            if (signal && signal.aborted) {
                reject(createAbortError(signal.reason));
                return;
            }

            let id = null;
            let cleanup = () => {};

            const onAbort = () => {
                if (id != null) {
                    globalThis.clearImmediate(id);
                }
                cleanup();
                reject(createAbortError(signal.reason));
            };

            cleanup = addAbortListener(signal, onAbort);

            id = globalThis.setImmediate(() => {
                cleanup();
                resolve(value);
            });
            applyRef('immediate', id, refed);
        });
    }

    function promiseInterval(delay, value, options) {
        const signal = options && options.signal;
        const refed = resolveRefOption(options);
        const intervalMs = normalizeDelay(delay);

        let id = null;
        let done = false;
        let queued = [];
        let waiters = [];
        let abortError = null;
        let removeAbort = () => {};

        function drainWaiters(err) {
            const pending = waiters;
            waiters = [];
            for (const waiter of pending) {
                if (err) {
                    waiter.reject(err);
                } else {
                    waiter.resolve({ value: undefined, done: true });
                }
            }
        }

        function cleanup(err) {
            if (done) {
                return;
            }
            done = true;
            if (id != null) {
                globalThis.clearInterval(id);
                id = null;
            }
            removeAbort();
            if (err) {
                abortError = err;
            }
            drainWaiters(err);
            queued = [];
        }

        function onTick() {
            if (done) {
                return;
            }
            if (waiters.length > 0) {
                const waiter = waiters.shift();
                waiter.resolve({ value, done: false });
                return;
            }
            queued.push(value);
        }

        if (signal && signal.aborted) {
            abortError = createAbortError(signal.reason);
            done = true;
        } else {
            if (signal) {
                removeAbort = addAbortListener(signal, () => {
                    cleanup(createAbortError(signal.reason));
                });
            }
            id = globalThis.setInterval(onTick, intervalMs);
            applyRef('interval', id, refed);
        }

        return {
            [Symbol.asyncIterator]() {
                return this;
            },
            next() {
                if (abortError) {
                    return Promise.reject(abortError);
                }
                if (done) {
                    return Promise.resolve({ value: undefined, done: true });
                }
                if (queued.length > 0) {
                    return Promise.resolve({ value: queued.shift(), done: false });
                }
                return new Promise((resolve, reject) => {
                    waiters.push({ resolve, reject });
                });
            },
            return() {
                cleanup();
                return Promise.resolve({ value: undefined, done: true });
            },
            throw(err) {
                cleanup(err || new Error('Iterator throw'));
                return Promise.reject(err);
            },
        };
    }

    const timersPromises = {
        setTimeout: (delay, value, options) => promiseDelay(delay, value, options),
        setImmediate: (value, options) => promiseImmediate(value, options),
        setInterval: (delay, value, options) => promiseInterval(delay, value, options),
    };

    const timersModule = {
        setTimeout: setTimeoutCompat,
        clearTimeout: clearTimeoutCompat,
        setInterval: setIntervalCompat,
        clearInterval: clearIntervalCompat,
        setImmediate: setImmediateCompat,
        clearImmediate: clearImmediateCompat,
        promises: timersPromises,
    };

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('timers', timersModule);
        globalThis.__registerNodeBuiltin('timers/promises', timersPromises);
    }

    globalThis.__timersModule = timersModule;
})();
