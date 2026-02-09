// Node.js stream module - ESM export wrapper (stub)
// Full stream implementation would require more native support

import { EventEmitter } from 'node:events';

export class Readable extends EventEmitter {
    constructor(options = {}) {
        super();
        this._readableState = {
            flowing: false,
            ended: false,
            buffer: [],
        };
    }

    read(size) {
        // Stub implementation
        return null;
    }

    push(chunk) {
        if (chunk === null) {
            this._readableState.ended = true;
            this.emit('end');
            return false;
        }
        this._readableState.buffer.push(chunk);
        this.emit('data', chunk);
        return true;
    }

    pipe(destination) {
        this.on('data', (chunk) => destination.write(chunk));
        this.on('end', () => destination.end());
        return destination;
    }
}

export class Writable extends EventEmitter {
    constructor(options = {}) {
        super();
        this._writableState = {
            ended: false,
        };
    }

    write(chunk, encoding, callback) {
        if (typeof encoding === 'function') {
            callback = encoding;
            encoding = 'utf8';
        }
        this.emit('write', chunk);
        if (callback) callback();
        return true;
    }

    end(chunk, encoding, callback) {
        if (chunk) this.write(chunk, encoding);
        this._writableState.ended = true;
        this.emit('finish');
        if (typeof callback === 'function') callback();
    }
}

export class Duplex extends Readable {
    constructor(options = {}) {
        super(options);
        this._writableState = { ended: false };
    }

    write(chunk, encoding, callback) {
        return Writable.prototype.write.call(this, chunk, encoding, callback);
    }

    end(chunk, encoding, callback) {
        return Writable.prototype.end.call(this, chunk, encoding, callback);
    }
}

export class Transform extends Duplex {
    constructor(options = {}) {
        super(options);
    }

    _transform(chunk, encoding, callback) {
        callback(null, chunk);
    }
}

export class PassThrough extends Transform { }

export default {
    Readable,
    Writable,
    Duplex,
    Transform,
    PassThrough,
};
