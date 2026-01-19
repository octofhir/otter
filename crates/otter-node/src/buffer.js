// node:buffer module implementation
// Buffer class for binary data manipulation - Node.js 24 compatible

(function() {
    'use strict';

    // Constants
    const kMaxLength = 0x7fffffff; // 2GB - 1
    const kStringMaxLength = 0x3fffffe7; // ~1GB

    // Supported encodings
    const ENCODINGS = ['utf8', 'utf-8', 'hex', 'base64', 'ascii', 'latin1', 'binary', 'ucs2', 'ucs-2', 'utf16le', 'utf-16le'];

    // Buffer class
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
            if (value instanceof Uint8Array || value instanceof ArrayBuffer) {
                this.type = 'Buffer';
                this.data = Array.from(new Uint8Array(value));
                return;
            }
            const empty = alloc(0, 0);
            this.type = 'Buffer';
            this.data = empty.data;
        }

        // Static methods
        static alloc(size, fill, encoding) {
            if (typeof size !== 'number' || size < 0) {
                throw new RangeError('The value of "size" is out of range');
            }
            const buf = new Buffer(alloc(size, 0));
            if (fill !== undefined) {
                buf.fill(fill, 0, size, encoding);
            }
            return buf;
        }

        static allocUnsafe(size) {
            if (typeof size !== 'number' || size < 0) {
                throw new RangeError('The value of "size" is out of range');
            }
            return new Buffer(alloc(size, 0));
        }

        static allocUnsafeSlow(size) {
            if (typeof size !== 'number' || size < 0) {
                throw new RangeError('The value of "size" is out of range');
            }
            return new Buffer(alloc(size, 0));
        }

        static from(data, encodingOrOffset, length) {
            if (data && data.type === 'Buffer' && Array.isArray(data.data)) {
                return new Buffer(data);
            }
            if (data && data.data && Array.isArray(data.data)) {
                return new Buffer({ type: 'Buffer', data: data.data });
            }
            if (data instanceof ArrayBuffer) {
                const offset = encodingOrOffset || 0;
                const len = length !== undefined ? length : data.byteLength - offset;
                return new Buffer({ type: 'Buffer', data: Array.from(new Uint8Array(data, offset, len)) });
            }
            if (ArrayBuffer.isView(data)) {
                return new Buffer({ type: 'Buffer', data: Array.from(new Uint8Array(data.buffer, data.byteOffset, data.byteLength)) });
            }
            if (Array.isArray(data)) {
                return new Buffer({ type: 'Buffer', data: data.map(n => n & 0xff) });
            }
            return new Buffer(from(data, encodingOrOffset || 'utf8'));
        }

        static concat(list, totalLength) {
            if (!Array.isArray(list)) {
                throw new TypeError('"list" argument must be an Array of Buffers');
            }
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

        static isEncoding(encoding) {
            if (typeof encoding !== 'string') return false;
            return ENCODINGS.includes(encoding.toLowerCase());
        }

        static byteLength(value, encoding) {
            if (Buffer.isBuffer(value)) {
                return value.length;
            }
            return byteLength(value, encoding || 'utf8');
        }

        static compare(buf1, buf2) {
            if (!Buffer.isBuffer(buf1) || !Buffer.isBuffer(buf2)) {
                throw new TypeError('Arguments must be Buffers');
            }
            return compare(buf1, buf2);
        }

        static poolSize = 8192;

        // Instance methods
        get length() {
            return this.data.length;
        }

        toString(encoding, start, end) {
            const len = this.data.length;
            const s = start ?? 0;
            const e = end ?? len;
            return toString(this, encoding || 'utf8', s, e);
        }

        toJSON() {
            return {
                type: 'Buffer',
                data: this.data.slice()
            };
        }

        slice(start, end) {
            return new Buffer(slice(this, start, end));
        }

        subarray(start, end) {
            const len = this.data.length;
            let s = start ?? 0;
            let e = end ?? len;
            if (s < 0) s = Math.max(len + s, 0);
            if (e < 0) e = Math.max(len + e, 0);
            s = Math.min(s, len);
            e = Math.min(e, len);
            return new Buffer({ type: 'Buffer', data: this.data.slice(s, e) });
        }

        equals(other) {
            return equals(this, other);
        }

        compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
            if (!Buffer.isBuffer(target)) {
                throw new TypeError('Argument must be a Buffer');
            }
            targetStart = targetStart ?? 0;
            targetEnd = targetEnd ?? target.length;
            sourceStart = sourceStart ?? 0;
            sourceEnd = sourceEnd ?? this.length;

            const sourceSlice = this.subarray(sourceStart, sourceEnd);
            const targetSlice = target.subarray(targetStart, targetEnd);
            return compare(sourceSlice, targetSlice);
        }

        copy(target, targetStart, sourceStart, sourceEnd) {
            const result = copy(this, target, targetStart ?? 0, sourceStart ?? 0, sourceEnd ?? this.length);
            target.data = result.targetData;
            return result.copied;
        }

        fill(value, offset, end, encoding) {
            offset = offset ?? 0;
            end = end ?? this.length;
            encoding = encoding ?? 'utf8';
            const result = fill(this, value, offset, end, encoding);
            this.data = result.data;
            return this;
        }

        write(string, offset, length, encoding) {
            if (typeof offset === 'string') {
                encoding = offset;
                offset = 0;
                length = this.length;
            } else if (typeof length === 'string') {
                encoding = length;
                length = this.length - (offset || 0);
            }
            offset = offset ?? 0;
            length = length ?? (this.length - offset);
            encoding = encoding ?? 'utf8';
            const result = write(this, string, offset, length, encoding);
            this.data = result.data;
            return result.written;
        }

        indexOf(value, byteOffset, encoding) {
            return indexOf(this, value, byteOffset ?? 0, encoding ?? 'utf8');
        }

        lastIndexOf(value, byteOffset, encoding) {
            return lastIndexOf(this, value, byteOffset ?? this.length, encoding ?? 'utf8');
        }

        includes(value, byteOffset, encoding) {
            return includes(this, value, byteOffset ?? 0, encoding ?? 'utf8');
        }

        swap16() {
            const result = swap16(this);
            this.data = result.data;
            return this;
        }

        swap32() {
            const result = swap32(this);
            this.data = result.data;
            return this;
        }

        swap64() {
            const result = swap64(this);
            this.data = result.data;
            return this;
        }

        // Read methods
        readUInt8(offset) {
            return readUInt8(this, offset ?? 0);
        }

        readUint8(offset) {
            return this.readUInt8(offset);
        }

        readInt8(offset) {
            return readInt8(this, offset ?? 0);
        }

        readUInt16LE(offset) {
            return readUInt16LE(this, offset ?? 0);
        }

        readUint16LE(offset) {
            return this.readUInt16LE(offset);
        }

        readUInt16BE(offset) {
            return readUInt16BE(this, offset ?? 0);
        }

        readUint16BE(offset) {
            return this.readUInt16BE(offset);
        }

        readInt16LE(offset) {
            return readInt16LE(this, offset ?? 0);
        }

        readInt16BE(offset) {
            return readInt16BE(this, offset ?? 0);
        }

        readUInt32LE(offset) {
            return readUInt32LE(this, offset ?? 0);
        }

        readUint32LE(offset) {
            return this.readUInt32LE(offset);
        }

        readUInt32BE(offset) {
            return readUInt32BE(this, offset ?? 0);
        }

        readUint32BE(offset) {
            return this.readUInt32BE(offset);
        }

        readInt32LE(offset) {
            return readInt32LE(this, offset ?? 0);
        }

        readInt32BE(offset) {
            return readInt32BE(this, offset ?? 0);
        }

        readFloatLE(offset) {
            return readFloatLE(this, offset ?? 0);
        }

        readFloatBE(offset) {
            return readFloatBE(this, offset ?? 0);
        }

        readDoubleLE(offset) {
            return readDoubleLE(this, offset ?? 0);
        }

        readDoubleBE(offset) {
            return readDoubleBE(this, offset ?? 0);
        }

        readBigInt64LE(offset) {
            return BigInt(readBigInt64LE(this, offset ?? 0));
        }

        readBigInt64BE(offset) {
            return BigInt(readBigInt64BE(this, offset ?? 0));
        }

        readBigUInt64LE(offset) {
            return BigInt(readBigUInt64LE(this, offset ?? 0));
        }

        readBigUint64LE(offset) {
            return this.readBigUInt64LE(offset);
        }

        readBigUInt64BE(offset) {
            return BigInt(readBigUInt64BE(this, offset ?? 0));
        }

        readBigUint64BE(offset) {
            return this.readBigUInt64BE(offset);
        }

        // Write methods
        writeUInt8(value, offset) {
            const result = writeUInt8(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUint8(value, offset) {
            return this.writeUInt8(value, offset);
        }

        writeInt8(value, offset) {
            const result = writeInt8(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUInt16LE(value, offset) {
            const result = writeUInt16LE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUint16LE(value, offset) {
            return this.writeUInt16LE(value, offset);
        }

        writeUInt16BE(value, offset) {
            const result = writeUInt16BE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUint16BE(value, offset) {
            return this.writeUInt16BE(value, offset);
        }

        writeInt16LE(value, offset) {
            const result = writeInt16LE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeInt16BE(value, offset) {
            const result = writeInt16BE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUInt32LE(value, offset) {
            const result = writeUInt32LE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUint32LE(value, offset) {
            return this.writeUInt32LE(value, offset);
        }

        writeUInt32BE(value, offset) {
            const result = writeUInt32BE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeUint32BE(value, offset) {
            return this.writeUInt32BE(value, offset);
        }

        writeInt32LE(value, offset) {
            const result = writeInt32LE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeInt32BE(value, offset) {
            const result = writeInt32BE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeFloatLE(value, offset) {
            const result = writeFloatLE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeFloatBE(value, offset) {
            const result = writeFloatBE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeDoubleLE(value, offset) {
            const result = writeDoubleLE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeDoubleBE(value, offset) {
            const result = writeDoubleBE(this, value, offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeBigInt64LE(value, offset) {
            const result = writeBigInt64LE(this, String(value), offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeBigInt64BE(value, offset) {
            const result = writeBigInt64BE(this, String(value), offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeBigUInt64LE(value, offset) {
            const result = writeBigUInt64LE(this, String(value), offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeBigUint64LE(value, offset) {
            return this.writeBigUInt64LE(value, offset);
        }

        writeBigUInt64BE(value, offset) {
            const result = writeBigUInt64BE(this, String(value), offset ?? 0);
            this.data = result.data;
            return result.offset;
        }

        writeBigUint64BE(value, offset) {
            return this.writeBigUInt64BE(value, offset);
        }

        // Iterators
        [Symbol.iterator]() {
            return this.values();
        }

        keys() {
            let index = 0;
            const data = this.data;
            return {
                [Symbol.iterator]() { return this; },
                next() {
                    if (index < data.length) {
                        return { value: index++, done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        }

        values() {
            let index = 0;
            const data = this.data;
            return {
                [Symbol.iterator]() { return this; },
                next() {
                    if (index < data.length) {
                        return { value: data[index++], done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        }

        entries() {
            let index = 0;
            const data = this.data;
            return {
                [Symbol.iterator]() { return this; },
                next() {
                    if (index < data.length) {
                        return { value: [index, data[index++]], done: false };
                    }
                    return { value: undefined, done: true };
                }
            };
        }
    }

    // SlowBuffer - deprecated but still used by some packages
    class SlowBuffer extends Buffer {
        constructor(size) {
            super(alloc(size, 0));
        }
    }

    // Constants
    const constants = {
        MAX_LENGTH: kMaxLength,
        MAX_STRING_LENGTH: kStringMaxLength
    };

    // Blob class (Node.js 15+)
    class Blob {
        constructor(blobParts = [], options = {}) {
            this._parts = [];
            this._type = options.type || '';

            for (const part of blobParts) {
                if (typeof part === 'string') {
                    this._parts.push(new TextEncoder().encode(part));
                } else if (part instanceof Blob) {
                    this._parts.push(part._data());
                } else if (part instanceof ArrayBuffer) {
                    this._parts.push(new Uint8Array(part));
                } else if (ArrayBuffer.isView(part)) {
                    this._parts.push(new Uint8Array(part.buffer, part.byteOffset, part.byteLength));
                } else if (Buffer.isBuffer(part)) {
                    this._parts.push(new Uint8Array(part.data));
                }
            }
        }

        get size() {
            return this._parts.reduce((acc, part) => acc + part.length, 0);
        }

        get type() {
            return this._type;
        }

        _data() {
            const size = this.size;
            const result = new Uint8Array(size);
            let offset = 0;
            for (const part of this._parts) {
                result.set(part, offset);
                offset += part.length;
            }
            return result;
        }

        async text() {
            return new TextDecoder().decode(this._data());
        }

        async arrayBuffer() {
            return this._data().buffer;
        }

        slice(start, end, type) {
            const data = this._data();
            const sliced = data.slice(start, end);
            return new Blob([sliced], { type: type || this._type });
        }

        async stream() {
            const data = this._data();
            return new ReadableStream({
                start(controller) {
                    controller.enqueue(data);
                    controller.close();
                }
            });
        }
    }

    // File class (Node.js 20+) - extends Blob with filename and lastModified
    class File extends Blob {
        constructor(fileBits, fileName, options = {}) {
            super(fileBits, options);
            this._name = String(fileName);
            this._lastModified = options.lastModified !== undefined
                ? Number(options.lastModified)
                : Date.now();
        }

        get name() {
            return this._name;
        }

        get lastModified() {
            return this._lastModified;
        }

        get webkitRelativePath() {
            return '';
        }
    }

    // atob/btoa utilities
    function atob(data) {
        return Buffer.from(data, 'base64').toString('binary');
    }

    function btoa(data) {
        return Buffer.from(data, 'binary').toString('base64');
    }

    // transcode function
    function transcode(source, fromEnc, toEnc) {
        if (!Buffer.isBuffer(source)) {
            throw new TypeError('The "source" argument must be a Buffer');
        }
        const str = source.toString(fromEnc);
        return Buffer.from(str, toEnc);
    }

    // resolveObjectURL (stub)
    function resolveObjectURL(id) {
        return undefined;
    }

    // Register global Buffer
    globalThis.Buffer = Buffer;

    // Module exports
    const bufferModule = {
        Buffer,
        SlowBuffer,
        Blob,
        File,
        constants,
        kMaxLength,
        kStringMaxLength,
        atob,
        btoa,
        transcode,
        resolveObjectURL,
        INSPECT_MAX_BYTES: 50,
        isUtf8: (input) => {
            try {
                if (Buffer.isBuffer(input)) {
                    new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array(input.data));
                } else {
                    new TextDecoder('utf-8', { fatal: true }).decode(input);
                }
                return true;
            } catch {
                return false;
            }
        },
        isAscii: (input) => {
            const data = Buffer.isBuffer(input) ? input.data : Array.from(input);
            return data.every(byte => byte < 128);
        }
    };

    bufferModule.default = bufferModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('buffer', bufferModule);
    }
})();
