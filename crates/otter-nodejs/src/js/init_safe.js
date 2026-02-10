// Node.js API initialization for Otter VM (safe profile)
// This file intentionally avoids process and fs globals for embedded safety.

(function (global) {
    'use strict';

    // Buffer class
    global.Buffer = class Buffer extends Uint8Array {
        static alloc(size, fill = 0) {
            const bytes = __buffer_alloc(size, fill);
            return new Buffer(bytes);
        }

        static from(data, encoding = 'utf8') {
            if (typeof data === 'string') {
                const bytes = __buffer_from_string(data, encoding);
                return new Buffer(bytes);
            }
            if (Array.isArray(data) || data instanceof Uint8Array) {
                return new Buffer(data);
            }
            throw new TypeError('First argument must be string, Buffer, or Array');
        }

        static concat(list, totalLength) {
            if (!Array.isArray(list)) {
                throw new TypeError('list argument must be an Array');
            }
            if (list.length === 0) {
                return Buffer.alloc(0);
            }
            const total = totalLength ?? list.reduce((acc, buf) => acc + buf.length, 0);
            const result = Buffer.alloc(total);
            let offset = 0;
            for (const buf of list) {
                result.set(buf, offset);
                offset += buf.length;
            }
            return result;
        }

        static byteLength(string, encoding = 'utf8') {
            return __buffer_byte_length(string, encoding);
        }

        static isBuffer(obj) {
            return obj instanceof Buffer;
        }

        toString(encoding = 'utf8', start = 0, end = this.length) {
            const slice = Array.from(this.subarray(start, end));
            return __buffer_to_string(slice, encoding);
        }

        write(string, offset = 0, length, encoding = 'utf8') {
            const bytes = __buffer_from_string(string, encoding);
            const len = Math.min(bytes.length, length ?? this.length - offset);
            for (let i = 0; i < len; i++) {
                this[offset + i] = bytes[i];
            }
            return len;
        }

        copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
            const len = Math.min(sourceEnd - sourceStart, target.length - targetStart);
            for (let i = 0; i < len; i++) {
                target[targetStart + i] = this[sourceStart + i];
            }
            return len;
        }

        equals(other) {
            if (this.length !== other.length) return false;
            for (let i = 0; i < this.length; i++) {
                if (this[i] !== other[i]) return false;
            }
            return true;
        }
    };

    // EventEmitter class
    global.EventEmitter = class EventEmitter {
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
    };
})(globalThis);
