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
    const _getHashes = getHashes;
    const _getCiphers = getCiphers;
    const _getCurves = getCurves;
    const _timingSafeEqual = timingSafeEqual;
    const _pbkdf2 = pbkdf2;
    const _pbkdf2Sync = pbkdf2Sync;
    const _scrypt = scrypt;
    const _scryptSync = scryptSync;
    const _createCipheriv = createCipheriv;
    const _createDecipheriv = createDecipheriv;
    const _cipherUpdate = cipherUpdate;
    const _cipherFinal = cipherFinal;
    const _cipherSetAAD = cipherSetAAD;
    const _cipherGetAuthTag = cipherGetAuthTag;
    const _cipherSetAutoPadding = cipherSetAutoPadding;
    const _decipherUpdate = decipherUpdate;
    const _decipherFinal = decipherFinal;
    const _decipherSetAAD = decipherSetAAD;
    const _decipherSetAuthTag = decipherSetAuthTag;
    const _decipherSetAutoPadding = decipherSetAutoPadding;
    const _cryptoSign = crypto_sign;
    const _cryptoVerify = crypto_verify;
    const _cryptoGenerateKeyPairSync = crypto_generate_key_pair_sync;
    const _cryptoGenerateKeyPair = crypto_generate_key_pair;
    const _cryptoSubtleDigest = crypto_subtle_digest;
    const _cryptoSubtleEncryptAesGcm = crypto_subtle_encrypt_aes_gcm;
    const _cryptoSubtleDecryptAesGcm = crypto_subtle_decrypt_aes_gcm;

    const Buffer = globalThis.Buffer || (globalThis.__otter_get_node_builtin
        ? globalThis.__otter_get_node_builtin('buffer').Buffer
        : undefined);

    function toBuffer(data, encoding) {
        if (!Buffer) {
            throw new Error('Buffer is not available for crypto');
        }
        if (Buffer.isBuffer(data)) {
            return data;
        }
        if (data instanceof ArrayBuffer) {
            return Buffer.from(data);
        }
        if (ArrayBuffer.isView(data)) {
            return Buffer.from(data.buffer, data.byteOffset, data.byteLength);
        }
        if (typeof data === 'string') {
            return Buffer.from(data, encoding || 'utf8');
        }
        return Buffer.from(data);
    }

    function fromBufferResult(result, encoding) {
        if (!result) {
            return Buffer.alloc(0);
        }
        if (result.type === 'Buffer') {
            const buf = Buffer.from(result.data || []);
            return encoding ? buf.toString(encoding) : buf;
        }
        if (Array.isArray(result)) {
            const buf = Buffer.from(result);
            return encoding ? buf.toString(encoding) : buf;
        }
        return encoding ? Buffer.from(result).toString(encoding) : Buffer.from(result);
    }

    function toArrayBuffer(buffer) {
        const view = Buffer.isBuffer(buffer) ? buffer : Buffer.from(buffer);
        return view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength);
    }

    function normalizeKeyInput(input) {
        let options = {};
        let key = input;
        if (input && typeof input === 'object' && input.key !== undefined) {
            options = input;
            key = input.key;
        }
        if (options.passphrase) {
            throw new Error('Encrypted keys are not supported');
        }
        const format = options.format || (typeof key === 'string' && key.includes('BEGIN') ? 'pem' : 'der');
        const type = options.type;
        return {
            key: toBuffer(key),
            format,
            type,
        };
    }

    function normalizeSignOptions(input) {
        if (!input || typeof input !== 'object') {
            return {};
        }
        const options = {};
        if (input.dsaEncoding) {
            options.dsaEncoding = input.dsaEncoding;
        }
        if (input.saltLength !== undefined) {
            options.saltLength = input.saltLength;
        }
        if (input.padding !== undefined) {
            options.padding = input.padding;
        }
        return options;
    }

    function normalizeKeyOutput(value) {
        if (!value) {
            return value;
        }
        if (typeof value === 'string') {
            return value;
        }
        if (value.type === 'Buffer') {
            return Buffer.from(value.data || []);
        }
        return value;
    }

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

    class Sign {
        constructor(algorithm) {
            this._algorithm = algorithm;
            this._chunks = [];
        }

        update(data, inputEncoding) {
            this._chunks.push(toBuffer(data, inputEncoding));
            return this;
        }

        sign(key, outputEncoding) {
            const keyInput = normalizeKeyInput(key);
            const options = normalizeSignOptions(key);
            const payload = Buffer.concat(this._chunks);
            const result = _cryptoSign(this._algorithm, keyInput, payload, options);
            return fromBufferResult(result, outputEncoding);
        }
    }

    class Verify {
        constructor(algorithm) {
            this._algorithm = algorithm;
            this._chunks = [];
        }

        update(data, inputEncoding) {
            this._chunks.push(toBuffer(data, inputEncoding));
            return this;
        }

        verify(key, signature, signatureEncoding) {
            const keyInput = normalizeKeyInput(key);
            const options = normalizeSignOptions(key);
            const payload = Buffer.concat(this._chunks);
            const sig = toBuffer(signature, signatureEncoding);
            return _cryptoVerify(this._algorithm, keyInput, payload, sig, options);
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

    globalThis.crypto.getHashes = function() {
        return _getHashes();
    };

    globalThis.crypto.getCiphers = function() {
        return _getCiphers();
    };

    globalThis.crypto.getCurves = function() {
        return _getCurves();
    };

    globalThis.crypto.timingSafeEqual = function(a, b) {
        return _timingSafeEqual(toBuffer(a), toBuffer(b));
    };

    globalThis.crypto.pbkdf2 = function(password, salt, iterations, keylen, digest, callback) {
        if (typeof digest === 'function') {
            callback = digest;
            digest = undefined;
        }
        const promise = _pbkdf2(
            toBuffer(password),
            toBuffer(salt),
            iterations,
            keylen,
            digest || 'sha1'
        );
        if (callback) {
            promise.then(
                (result) => callback(null, fromBufferResult(result)),
                (err) => callback(err)
            );
            return;
        }
        return promise.then((result) => fromBufferResult(result));
    };

    globalThis.crypto.pbkdf2Sync = function(password, salt, iterations, keylen, digest) {
        const result = _pbkdf2Sync(
            toBuffer(password),
            toBuffer(salt),
            iterations,
            keylen,
            digest || 'sha1'
        );
        return fromBufferResult(result);
    };

    globalThis.crypto.scrypt = function(password, salt, keylen, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = undefined;
        }
        const promise = _scrypt(
            toBuffer(password),
            toBuffer(salt),
            keylen,
            options || {}
        );
        if (callback) {
            promise.then(
                (result) => callback(null, fromBufferResult(result)),
                (err) => callback(err)
            );
            return;
        }
        return promise.then((result) => fromBufferResult(result));
    };

    globalThis.crypto.scryptSync = function(password, salt, keylen, options) {
        const result = _scryptSync(
            toBuffer(password),
            toBuffer(salt),
            keylen,
            options || {}
        );
        return fromBufferResult(result);
    };

    globalThis.crypto.createSign = function(algorithm) {
        return new Sign(algorithm);
    };

    globalThis.crypto.createVerify = function(algorithm) {
        return new Verify(algorithm);
    };

    globalThis.crypto.sign = function(algorithm, data, key) {
        const signer = new Sign(algorithm);
        signer.update(data);
        return signer.sign(key);
    };

    globalThis.crypto.verify = function(algorithm, data, key, signature) {
        const verifier = new Verify(algorithm);
        verifier.update(data);
        return verifier.verify(key, signature);
    };

    globalThis.crypto.generateKeyPair = function(type, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = {};
        }
        const promise = _cryptoGenerateKeyPair(type, options || {});
        if (callback) {
            promise.then(
                (result) => callback(null, normalizeKeyOutput(result.publicKey), normalizeKeyOutput(result.privateKey)),
                (err) => callback(err)
            );
            return;
        }
        return promise.then((result) => ({
            publicKey: normalizeKeyOutput(result.publicKey),
            privateKey: normalizeKeyOutput(result.privateKey),
        }));
    };

    globalThis.crypto.generateKeyPairSync = function(type, options) {
        const result = _cryptoGenerateKeyPairSync(type, options || {});
        return {
            publicKey: normalizeKeyOutput(result.publicKey),
            privateKey: normalizeKeyOutput(result.privateKey),
        };
    };

    class CipherBase {
        constructor(id, decrypt) {
            this._id = id;
            this._decrypt = decrypt;
        }

        update(data, inputEncoding, outputEncoding) {
            const bytes = toBuffer(data, inputEncoding);
            const result = this._decrypt
                ? _decipherUpdate(this._id, bytes)
                : _cipherUpdate(this._id, bytes);
            return fromBufferResult(result, outputEncoding);
        }

        final(outputEncoding) {
            const result = this._decrypt ? _decipherFinal(this._id) : _cipherFinal(this._id);
            if (!this._decrypt && result && result.authTag) {
                this._authTag = fromBufferResult(result.authTag);
            }
            return fromBufferResult(result, outputEncoding);
        }

        setAAD(aad) {
            const bytes = toBuffer(aad);
            if (this._decrypt) {
                _decipherSetAAD(this._id, bytes);
            } else {
                _cipherSetAAD(this._id, bytes);
            }
            return this;
        }

        setAutoPadding(value) {
            const enabled = value !== false;
            if (this._decrypt) {
                _decipherSetAutoPadding(this._id, enabled);
            } else {
                _cipherSetAutoPadding(this._id, enabled);
            }
            return enabled;
        }

        getAuthTag() {
            if (this._decrypt) {
                throw new Error('getAuthTag is not supported on Decipher');
            }
            if (this._authTag) {
                return this._authTag;
            }
            return fromBufferResult(_cipherGetAuthTag(this._id));
        }
    }

    class Cipher extends CipherBase {
        constructor(id) {
            super(id, false);
        }
    }

    class Decipher extends CipherBase {
        constructor(id) {
            super(id, true);
        }

        setAuthTag(tag) {
            _decipherSetAuthTag(this._id, toBuffer(tag));
            return this;
        }
    }

    globalThis.crypto.createCipheriv = function(algorithm, key, iv, options) {
        const id = _createCipheriv(algorithm, toBuffer(key), toBuffer(iv), options || {});
        return new Cipher(id);
    };

    globalThis.crypto.createDecipheriv = function(algorithm, key, iv, options) {
        const id = _createDecipheriv(algorithm, toBuffer(key), toBuffer(iv), options || {});
        return new Decipher(id);
    };

    const kKeyData = Symbol('keyData');
    const kKeyFormat = Symbol('keyFormat');
    const kKeyType = Symbol('keyType');

    class CryptoKey {
        constructor(type, algorithm, extractable, usages, data, format, keyType) {
            this.type = type;
            this.algorithm = algorithm;
            this.extractable = !!extractable;
            this.usages = Array.isArray(usages) ? usages.slice() : [];
            this[kKeyData] = data;
            this[kKeyFormat] = format;
            this[kKeyType] = keyType;
        }
    }

    function resolveHashName(value) {
        if (!value) {
            return 'sha256';
        }
        const name = typeof value === 'string' ? value : value.name;
        return String(name || '').toLowerCase();
    }

    function signAlgorithmName(algorithm) {
        const name = String(algorithm.name || algorithm).toUpperCase();
        const hash = resolveHashName(algorithm.hash);
        if (name === 'RSA-PSS') {
            return `rsa-pss-${hash}`;
        }
        if (name === 'RSASSA-PKCS1-V1_5') {
            return `rsa-${hash}`;
        }
        if (name === 'ECDSA') {
            return `ecdsa-with-${hash}`;
        }
        return String(algorithm);
    }

    function createCryptoKey(kind, algorithm, extractable, usages, data, format, keyType) {
        return new CryptoKey(kind, algorithm, extractable, usages, data, format, keyType);
    }

    const subtle = {
        async digest(algorithm, data) {
            const name = resolveHashName(algorithm);
            const result = _cryptoSubtleDigest(name, toBuffer(data));
            return toArrayBuffer(fromBufferResult(result));
        },
        async sign(algorithm, key, data) {
            if (!key || !(key instanceof CryptoKey)) {
                throw new Error('Invalid CryptoKey');
            }
            if (key.algorithm && key.algorithm.name === 'HMAC') {
                const hash = resolveHashName(key.algorithm.hash);
                const hmac = globalThis.crypto.createHmac(hash, key[kKeyData]);
                hmac.update(toBuffer(data));
                return toArrayBuffer(hmac.digest());
            }
            const options = {};
            if (algorithm && algorithm.saltLength !== undefined) {
                options.saltLength = algorithm.saltLength;
            }
            if (algorithm && algorithm.dsaEncoding) {
                options.dsaEncoding = algorithm.dsaEncoding;
            }
            const signature = _cryptoSign(
                signAlgorithmName(algorithm),
                { key: key[kKeyData], format: key[kKeyFormat], type: key[kKeyType] },
                toBuffer(data),
                options
            );
            return toArrayBuffer(fromBufferResult(signature));
        },
        async verify(algorithm, key, signature, data) {
            if (!key || !(key instanceof CryptoKey)) {
                throw new Error('Invalid CryptoKey');
            }
            if (key.algorithm && key.algorithm.name === 'HMAC') {
                const hash = resolveHashName(key.algorithm.hash);
                const hmac = globalThis.crypto.createHmac(hash, key[kKeyData]);
                hmac.update(toBuffer(data));
                const digest = hmac.digest();
                return globalThis.crypto.timingSafeEqual(digest, toBuffer(signature));
            }
            const options = {};
            if (algorithm && algorithm.saltLength !== undefined) {
                options.saltLength = algorithm.saltLength;
            }
            if (algorithm && algorithm.dsaEncoding) {
                options.dsaEncoding = algorithm.dsaEncoding;
            }
            return _cryptoVerify(
                signAlgorithmName(algorithm),
                { key: key[kKeyData], format: key[kKeyFormat], type: key[kKeyType] },
                toBuffer(data),
                toBuffer(signature),
                options
            );
        },
        async encrypt(algorithm, key, data) {
            const name = String(algorithm.name || algorithm).toUpperCase();
            if (name !== 'AES-GCM') {
                throw new Error(`Unsupported algorithm ${name}`);
            }
            const result = _cryptoSubtleEncryptAesGcm(
                key[kKeyData],
                toBuffer(data),
                {
                    iv: toBuffer(algorithm.iv),
                    additionalData: algorithm.additionalData ? toBuffer(algorithm.additionalData) : undefined,
                    tagLength: algorithm.tagLength,
                }
            );
            return toArrayBuffer(fromBufferResult(result));
        },
        async decrypt(algorithm, key, data) {
            const name = String(algorithm.name || algorithm).toUpperCase();
            if (name !== 'AES-GCM') {
                throw new Error(`Unsupported algorithm ${name}`);
            }
            const result = _cryptoSubtleDecryptAesGcm(
                key[kKeyData],
                toBuffer(data),
                {
                    iv: toBuffer(algorithm.iv),
                    additionalData: algorithm.additionalData ? toBuffer(algorithm.additionalData) : undefined,
                    tagLength: algorithm.tagLength,
                }
            );
            return toArrayBuffer(fromBufferResult(result));
        },
        async generateKey(algorithm, extractable, usages) {
            const name = String(algorithm.name || algorithm).toUpperCase();
            if (name === 'AES-GCM' || name === 'HMAC') {
                const length = algorithm.length || 256;
                const bytes = globalThis.crypto.randomBytes(Math.ceil(length / 8));
                return createCryptoKey('secret', { name, length, hash: algorithm.hash }, extractable, usages, bytes, 'raw', 'raw');
            }
            if (name === 'RSA-PSS' || name === 'RSASSA-PKCS1-V1_5') {
                const result = await _cryptoGenerateKeyPair('rsa', {
                    modulusLength: algorithm.modulusLength || 2048,
                    publicExponent: algorithm.publicExponent || 65537,
                    publicKeyEncoding: { format: 'der', type: 'spki' },
                    privateKeyEncoding: { format: 'der', type: 'pkcs8' },
                });
                const publicKey = createCryptoKey(
                    'public',
                    { name, hash: algorithm.hash },
                    true,
                    usages,
                    normalizeKeyOutput(result.publicKey),
                    'der',
                    'spki'
                );
                const privateKey = createCryptoKey(
                    'private',
                    { name, hash: algorithm.hash },
                    extractable,
                    usages,
                    normalizeKeyOutput(result.privateKey),
                    'der',
                    'pkcs8'
                );
                return { publicKey, privateKey };
            }
            if (name === 'ECDSA') {
                const result = await _cryptoGenerateKeyPair('ec', {
                    namedCurve: algorithm.namedCurve || 'prime256v1',
                    publicKeyEncoding: { format: 'der', type: 'spki' },
                    privateKeyEncoding: { format: 'der', type: 'pkcs8' },
                });
                const publicKey = createCryptoKey(
                    'public',
                    { name, namedCurve: algorithm.namedCurve || 'prime256v1' },
                    true,
                    usages,
                    normalizeKeyOutput(result.publicKey),
                    'der',
                    'spki'
                );
                const privateKey = createCryptoKey(
                    'private',
                    { name, namedCurve: algorithm.namedCurve || 'prime256v1' },
                    extractable,
                    usages,
                    normalizeKeyOutput(result.privateKey),
                    'der',
                    'pkcs8'
                );
                return { publicKey, privateKey };
            }
            throw new Error(`Unsupported algorithm ${name}`);
        },
        async importKey(format, keyData, algorithm, extractable, usages) {
            const name = String(algorithm.name || algorithm).toUpperCase();
            if (format === 'raw') {
                return createCryptoKey('secret', { name, hash: algorithm.hash, length: algorithm.length }, extractable, usages, toBuffer(keyData), 'raw', 'raw');
            }
            if (format === 'pkcs8') {
                return createCryptoKey('private', { name, hash: algorithm.hash, namedCurve: algorithm.namedCurve }, extractable, usages, toBuffer(keyData), 'der', 'pkcs8');
            }
            if (format === 'spki') {
                return createCryptoKey('public', { name, hash: algorithm.hash, namedCurve: algorithm.namedCurve }, true, usages, toBuffer(keyData), 'der', 'spki');
            }
            throw new Error(`Unsupported import format ${format}`);
        },
        async exportKey(format, key) {
            if (!key || !(key instanceof CryptoKey)) {
                throw new Error('Invalid CryptoKey');
            }
            if (!key.extractable) {
                throw new Error('Key is not extractable');
            }
            if (format === 'raw') {
                return toArrayBuffer(toBuffer(key[kKeyData]));
            }
            if (format === 'pkcs8' || format === 'spki') {
                return toArrayBuffer(toBuffer(key[kKeyData]));
            }
            throw new Error(`Unsupported export format ${format}`);
        },
    };

    globalThis.crypto.subtle = subtle;
    globalThis.crypto.webcrypto = {
        subtle,
        getRandomValues: globalThis.crypto.getRandomValues,
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
        createSign: globalThis.crypto.createSign,
        createVerify: globalThis.crypto.createVerify,
        sign: globalThis.crypto.sign,
        verify: globalThis.crypto.verify,
        generateKeyPair: globalThis.crypto.generateKeyPair,
        generateKeyPairSync: globalThis.crypto.generateKeyPairSync,
        hash: globalThis.crypto.hash,
        getHashes: globalThis.crypto.getHashes,
        getCiphers: globalThis.crypto.getCiphers,
        getCurves: globalThis.crypto.getCurves,
        timingSafeEqual: globalThis.crypto.timingSafeEqual,
        pbkdf2: globalThis.crypto.pbkdf2,
        pbkdf2Sync: globalThis.crypto.pbkdf2Sync,
        scrypt: globalThis.crypto.scrypt,
        scryptSync: globalThis.crypto.scryptSync,
        createCipheriv: globalThis.crypto.createCipheriv,
        createDecipheriv: globalThis.crypto.createDecipheriv,
        webcrypto: globalThis.crypto.webcrypto,
        subtle: globalThis.crypto.subtle,
        Cipher,
        Decipher,
        Sign,
        Verify,
    };
    cryptoModule.default = cryptoModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('crypto', cryptoModule);
    }
})();
