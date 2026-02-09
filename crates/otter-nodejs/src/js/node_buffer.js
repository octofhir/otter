// Node.js Buffer module - ESM export wrapper

class Buffer extends Uint8Array {
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
}

export { Buffer };
export default { Buffer };
