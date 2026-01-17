(function() {
    const DEFAULT_MAX_LISTENERS = 10;

    class EventEmitter {
        constructor() {
            this._events = new Map();
            this._maxListeners = DEFAULT_MAX_LISTENERS;
        }

        addListener(event, listener) {
            return this.on(event, listener);
        }

        on(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            const listeners = this._events.get(event);
            listeners.push({ fn: listener, once: false });

            if (this._maxListeners > 0 && listeners.length > this._maxListeners) {
                console.warn(
                    `MaxListenersExceededWarning: Possible EventEmitter memory leak detected. ` +
                    `${listeners.length} ${event} listeners added. Use emitter.setMaxListeners() to increase limit`
                );
            }

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        once(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).push({ fn: listener, once: true });

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        prependListener(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).unshift({ fn: listener, once: false });

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        prependOnceListener(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).unshift({ fn: listener, once: true });

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        removeListener(event, listener) {
            return this.off(event, listener);
        }

        off(event, listener) {
            if (!this._events.has(event)) {
                return this;
            }

            const listeners = this._events.get(event);
            const index = listeners.findIndex(l => l.fn === listener);

            if (index !== -1) {
                listeners.splice(index, 1);

                if (event !== 'removeListener' && this._events.has('removeListener')) {
                    this.emit('removeListener', event, listener);
                }
            }

            return this;
        }

        removeAllListeners(event) {
            if (event === undefined) {
                const events = [...this._events.keys()];
                for (const e of events) {
                    if (e !== 'removeListener') {
                        this.removeAllListeners(e);
                    }
                }
                this._events.delete('removeListener');
            } else if (this._events.has(event)) {
                if (event !== 'removeListener' && this._events.has('removeListener')) {
                    const listeners = this._events.get(event);
                    for (const l of listeners) {
                        this.emit('removeListener', event, l.fn);
                    }
                }
                this._events.delete(event);
            }

            return this;
        }

        emit(event, ...args) {
            if (!this._events.has(event)) {
                if (event === 'error') {
                    const err = args[0];
                    if (err instanceof Error) {
                        throw err;
                    }
                    throw new Error('Unhandled error: ' + err);
                }
                return false;
            }

            const listeners = this._events.get(event);
            if (listeners.length === 0) {
                if (event === 'error') {
                    const err = args[0];
                    if (err instanceof Error) {
                        throw err;
                    }
                    throw new Error('Unhandled error: ' + err);
                }
                return false;
            }

            const toCall = [...listeners];
            this._events.set(event, listeners.filter(l => !l.once));

            for (const listener of toCall) {
                try {
                    listener.fn.apply(this, args);
                } catch (err) {
                    if (event !== 'error') {
                        this.emit('error', err);
                    } else {
                        throw err;
                    }
                }
            }

            return true;
        }

        listeners(event) {
            if (!this._events.has(event)) {
                return [];
            }
            return this._events.get(event).map(l => l.fn);
        }

        rawListeners(event) {
            if (!this._events.has(event)) {
                return [];
            }
            return [...this._events.get(event)];
        }

        listenerCount(event) {
            if (!this._events.has(event)) {
                return 0;
            }
            return this._events.get(event).length;
        }

        eventNames() {
            return [...this._events.keys()].filter(e => this._events.get(e).length > 0);
        }

        setMaxListeners(n) {
            if (typeof n !== 'number' || n < 0 || Number.isNaN(n)) {
                throw new RangeError('The "n" argument must be a non-negative number');
            }
            this._maxListeners = n;
            return this;
        }

        getMaxListeners() {
            return this._maxListeners;
        }

        static get defaultMaxListeners() {
            return DEFAULT_MAX_LISTENERS;
        }

        static set defaultMaxListeners(n) {
            console.warn('EventEmitter.defaultMaxListeners is read-only in Otter');
        }

        static once(emitter, event, options = {}) {
            return new Promise((resolve, reject) => {
                const signal = options?.signal;

                if (signal?.aborted) {
                    reject(new Error('The operation was aborted'));
                    return;
                }

                const listener = (...args) => {
                    if (errorListener) {
                        emitter.off('error', errorListener);
                    }
                    resolve(args);
                };

                let errorListener;
                if (event !== 'error') {
                    errorListener = (err) => {
                        emitter.off(event, listener);
                        reject(err);
                    };
                    emitter.once('error', errorListener);
                }

                emitter.once(event, listener);

                if (signal) {
                    signal.addEventListener('abort', () => {
                        emitter.off(event, listener);
                        if (errorListener) {
                            emitter.off('error', errorListener);
                        }
                        reject(new Error('The operation was aborted'));
                    }, { once: true });
                }
            });
        }

        static on(emitter, event, options = {}) {
            const signal = options?.signal;
            const unconsumedEvents = [];
            const unconsumedPromises = [];
            let finished = false;
            let error = null;

            const eventHandler = (...args) => {
                if (unconsumedPromises.length > 0) {
                    unconsumedPromises.shift().resolve({ value: args, done: false });
                } else {
                    unconsumedEvents.push(args);
                }
            };

            const errorHandler = (err) => {
                error = err;
                if (unconsumedPromises.length > 0) {
                    unconsumedPromises.shift().reject(err);
                }
            };

            emitter.on(event, eventHandler);
            if (event !== 'error') {
                emitter.on('error', errorHandler);
            }

            return {
                [Symbol.asyncIterator]() {
                    return this;
                },
                next() {
                    if (unconsumedEvents.length > 0) {
                        return Promise.resolve({ value: unconsumedEvents.shift(), done: false });
                    }

                    if (finished) {
                        return Promise.resolve({ done: true });
                    }

                    if (error) {
                        return Promise.reject(error);
                    }

                    return new Promise((resolve, reject) => {
                        unconsumedPromises.push({ resolve, reject });
                    });
                },
                return() {
                    finished = true;
                    emitter.off(event, eventHandler);
                    emitter.off('error', errorHandler);

                    for (const promise of unconsumedPromises) {
                        promise.resolve({ done: true });
                    }

                    return Promise.resolve({ done: true });
                },
                throw(err) {
                    error = err;
                    emitter.off(event, eventHandler);
                    emitter.off('error', errorHandler);
                    return Promise.reject(err);
                }
            };
        }

        static listenerCount(emitter, event) {
            if (typeof emitter.listenerCount === 'function') {
                return emitter.listenerCount(event);
            }
            return 0;
        }
    }

    globalThis.__EventEmitter = EventEmitter;

    const eventsModule = {
        EventEmitter,
        once: EventEmitter.once,
        on: EventEmitter.on,
        listenerCount: EventEmitter.listenerCount,
        default: EventEmitter,
    };

    if (globalThis.__registerModule) {
        globalThis.__registerModule('events', eventsModule);
        globalThis.__registerModule('node:events', eventsModule);
    }
})();
