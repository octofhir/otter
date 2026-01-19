// node:crypto wrapper - provides createHash, createHmac, randomBytes, etc.

(function() {
    'use strict';

    // Save references to native ops before any potential shadowing
    const _randomBytes = randomBytes;
    const _randomUUID = randomUUID;
    const _getRandomValues = getRandomValues;
    const _createHash = createHash;
    const _createHmac = createHmac;
    const _hashUpdate = hashUpdate;
    const _hashDigest = hashDigest;
    const _hmacUpdate = hmacUpdate;
    const _hmacDigest = hmacDigest;
    const _hash = hash;

    // Hash class
    class Hash {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            _hashUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return _hashDigest(this._id, encoding);
        }
    }

    // Hmac class
    class Hmac {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            _hmacUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return _hmacDigest(this._id, encoding);
        }
    }

    // Export crypto namespace
    globalThis.crypto = globalThis.crypto || {};

    // randomBytes(size, callback?)
    globalThis.crypto.randomBytes = function(size, callback) {
        const result = _randomBytes(size);
        if (callback) {
            // Async version - call immediately (already sync in Rust)
            setImmediate(() => callback(null, result));
            return;
        }
        return result;
    };

    // randomUUID()
    globalThis.crypto.randomUUID = function() {
        return _randomUUID();
    };

    // getRandomValues(typedArray) - Web Crypto API
    globalThis.crypto.getRandomValues = function(typedArray) {
        const bytes = _getRandomValues(typedArray.length);
        for (let i = 0; i < bytes.length; i++) {
            typedArray[i] = bytes[i];
        }
        return typedArray;
    };

    // createHash(algorithm)
    globalThis.crypto.createHash = function(algorithm) {
        const id = _createHash(algorithm);
        return new Hash(id);
    };

    // createHmac(algorithm, key)
    globalThis.crypto.createHmac = function(algorithm, key) {
        const id = _createHmac(algorithm, key);
        return new Hmac(id);
    };

    // hash(algorithm, data, encoding) - one-shot convenience
    globalThis.crypto.hash = function(algorithm, data, encoding) {
        return _hash(algorithm, data, encoding);
    };

    // Aliases for compatibility
    globalThis.randomBytes = globalThis.crypto.randomBytes;
    globalThis.randomUUID = globalThis.crypto.randomUUID;

    const cryptoModule = {
        randomBytes: globalThis.crypto.randomBytes,
        randomUUID: globalThis.crypto.randomUUID,
        getRandomValues: globalThis.crypto.getRandomValues,
        createHash: globalThis.crypto.createHash,
        createHmac: globalThis.crypto.createHmac,
        hash: globalThis.crypto.hash,
    };
    cryptoModule.default = cryptoModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('crypto', cryptoModule);
    }
})();
