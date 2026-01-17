// node:buffer module implementation
// Buffer class for binary data manipulation

(function() {
    'use strict';

    // Buffer is represented as: { type: 'Buffer', data: number[] }
    // This wrapper exposes a Node-like Buffer class and registers node:buffer.
    class Buffer {
        constructor(value) {
            if (value && value.type === 'Buffer' && Array.isArray(value.data)) {
                this.type = 'Buffer';
                this.data = value.data;
                return;
            }
            if (Array.isArray(value)) {
                this.type = 'Buffer';
                this.data = value.map((n) => n & 0xff);
                return;
            }
            const empty = alloc(0, 0);
            this.type = 'Buffer';
            this.data = empty.data;
        }

        static alloc(size, fill) {
            return new Buffer(alloc(size, fill ?? 0));
        }

        static from(data, encoding) {
            if (data && data.type === 'Buffer' && Array.isArray(data.data)) {
                return new Buffer(data);
            }
            if (data && data.data && Array.isArray(data.data)) {
                return new Buffer({ type: 'Buffer', data: data.data });
            }
            return new Buffer(from(data, encoding || 'utf8'));
        }

        static concat(list, totalLength) {
            const normalized = (list || []).map((v) => {
                if (v && v.type === 'Buffer') return v;
                if (v && v.data && Array.isArray(v.data)) return { type: 'Buffer', data: v.data };
                return Buffer.from(v);
            });
            return new Buffer(concat(normalized, totalLength));
        }

        static isBuffer(value) {
            return value && value.type === 'Buffer' && Array.isArray(value.data);
        }

        static byteLength(value, encoding) {
            return byteLength(value, encoding || 'utf8');
        }

        toString(encoding, start, end) {
            const len = this.data.length;
            const s = start ?? 0;
            const e = end ?? len;
            return toString(this, encoding || 'utf8', s, e);
        }

        slice(start, end) {
            return new Buffer(slice(this, start, end));
        }

        equals(other) {
            return equals(this, other);
        }

        compare(other) {
            return compare(this, other);
        }

        get length() {
            return this.data.length;
        }

        [Symbol.iterator]() {
            return this.data[Symbol.iterator]();
        }
    }

    globalThis.Buffer = Buffer;

    const bufferModule = { Buffer };
    bufferModule.default = bufferModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('buffer', bufferModule);
        globalThis.__registerModule('node:buffer', bufferModule);
    }
})();
