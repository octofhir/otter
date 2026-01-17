// node:crypto wrapper - provides createHash, createHmac, randomBytes, etc.

(function() {
    'use strict';

    // Hash class
    class Hash {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            hashUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return hashDigest(this._id, encoding);
        }
    }

    // Hmac class
    class Hmac {
        constructor(id) {
            this._id = id;
        }

        update(data) {
            hmacUpdate(this._id, data);
            return this;
        }

        digest(encoding) {
            return hmacDigest(this._id, encoding);
        }
    }

    // Export crypto namespace
    globalThis.crypto = globalThis.crypto || {};

    // randomBytes(size, callback?)
    globalThis.crypto.randomBytes = function(size, callback) {
        const result = randomBytes(size);
        if (callback) {
            // Async version - call immediately (already sync in Rust)
            setImmediate(() => callback(null, result));
            return;
        }
        return result;
    };

    // randomUUID()
    globalThis.crypto.randomUUID = function() {
        return randomUUID();
    };

    // getRandomValues(typedArray) - Web Crypto API
    globalThis.crypto.getRandomValues = function(typedArray) {
        const bytes = getRandomValues(typedArray.length);
        for (let i = 0; i < bytes.length; i++) {
            typedArray[i] = bytes[i];
        }
        return typedArray;
    };

    // createHash(algorithm)
    globalThis.crypto.createHash = function(algorithm) {
        const id = createHash(algorithm);
        return new Hash(id);
    };

    // createHmac(algorithm, key)
    globalThis.crypto.createHmac = function(algorithm, key) {
        const id = createHmac(algorithm, key);
        return new Hmac(id);
    };

    // hash(algorithm, data, encoding) - one-shot convenience
    globalThis.crypto.hash = function(algorithm, data, encoding) {
        return hash(algorithm, data, encoding);
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

    if (globalThis.__registerModule) {
        globalThis.__registerModule('crypto', cryptoModule);
        globalThis.__registerModule('node:crypto', cryptoModule);
    }
})();
