// Node.js events module - ESM export wrapper

export class EventEmitter {
    constructor() {
        this._events = new Map();
        this._maxListeners = 10;
    }

    on(event, listener) {
        if (!this._events.has(event)) {
            this._events.set(event, []);
        }
        this._events.get(event).push({ fn: listener, once: false });
        return this;
    }

    once(event, listener) {
        if (!this._events.has(event)) {
            this._events.set(event, []);
        }
        this._events.get(event).push({ fn: listener, once: true });
        return this;
    }

    off(event, listener) {
        return this.removeListener(event, listener);
    }

    removeListener(event, listener) {
        const listeners = this._events.get(event);
        if (!listeners) return this;

        const idx = listeners.findIndex(l => l.fn === listener);
        if (idx !== -1) {
            listeners.splice(idx, 1);
        }
        return this;
    }

    removeAllListeners(event) {
        if (event === undefined) {
            this._events.clear();
        } else {
            this._events.delete(event);
        }
        return this;
    }

    emit(event, ...args) {
        const listeners = this._events.get(event);
        if (!listeners || listeners.length === 0) {
            return false;
        }

        const toRemove = [];
        for (let i = 0; i < listeners.length; i++) {
            const { fn, once } = listeners[i];
            fn.apply(this, args);
            if (once) toRemove.push(i);
        }

        for (let i = toRemove.length - 1; i >= 0; i--) {
            listeners.splice(toRemove[i], 1);
        }

        return true;
    }

    listenerCount(event) {
        const listeners = this._events.get(event);
        return listeners ? listeners.length : 0;
    }

    listeners(event) {
        const listeners = this._events.get(event);
        return listeners ? listeners.map(l => l.fn) : [];
    }

    setMaxListeners(n) {
        this._maxListeners = n;
        return this;
    }

    getMaxListeners() {
        return this._maxListeners;
    }

    addListener(event, listener) {
        return this.on(event, listener);
    }

    prependListener(event, listener) {
        if (!this._events.has(event)) {
            this._events.set(event, []);
        }
        this._events.get(event).unshift({ fn: listener, once: false });
        return this;
    }

    eventNames() {
        return [...this._events.keys()];
    }
}

export default EventEmitter;
