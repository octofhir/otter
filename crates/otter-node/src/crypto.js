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
    const _cryptoSubtleEncryptAesCbc = crypto_subtle_encrypt_aes_cbc;
    const _cryptoSubtleDecryptAesCbc = crypto_subtle_decrypt_aes_cbc;
    const _cryptoSubtleEncryptAesCtr = crypto_subtle_encrypt_aes_ctr;
    const _cryptoSubtleDecryptAesCtr = crypto_subtle_decrypt_aes_ctr;
    const _cryptoSubtleWrapAesKw = crypto_subtle_wrap_aes_kw;
    const _cryptoSubtleUnwrapAesKw = crypto_subtle_unwrap_aes_kw;
    const _cryptoSubtleRsaOaepEncrypt = crypto_subtle_rsa_oaep_encrypt;
    const _cryptoSubtleRsaOaepDecrypt = crypto_subtle_rsa_oaep_decrypt;
    const _cryptoSubtleDeriveBitsEcdh = crypto_subtle_derive_bits_ecdh;
    const _cryptoSubtleDeriveBitsHkdf = crypto_subtle_derive_bits_hkdf;
    const _cryptoSubtleDeriveBitsPbkdf2 = crypto_subtle_derive_bits_pbkdf2;
    const _cryptoJwkToDer = crypto_jwk_to_der;
    const _cryptoDerToJwk = crypto_der_to_jwk;

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
        if (view && Array.isArray(view.data)) {
            const bytes = Uint8Array.from(view.data);
            return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
        }
        return view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength);
    }

    function base64UrlEncode(buffer) {
        return Buffer.from(buffer)
            .toString('base64')
            .replace(/\+/g, '-')
            .replace(/\//g, '_')
            .replace(/=+$/g, '');
    }

    function base64UrlDecode(text) {
        const padded = String(text || '').replace(/-/g, '+').replace(/_/g, '/');
        const pad = padded.length % 4 ? '='.repeat(4 - (padded.length % 4)) : '';
        return Buffer.from(padded + pad, 'base64');
    }

    function normalizeAlgorithmName(value) {
        const name = typeof value === 'string' ? value : value && value.name;
        return String(name || '').toUpperCase();
    }

    function ensureUsage(algorithm, keyType, usages) {
        const usageTable = {
            'AES-GCM': { secret: ['encrypt', 'decrypt', 'wrapKey', 'unwrapKey'] },
            'AES-CBC': { secret: ['encrypt', 'decrypt', 'wrapKey', 'unwrapKey'] },
            'AES-CTR': { secret: ['encrypt', 'decrypt', 'wrapKey', 'unwrapKey'] },
            'AES-KW': { secret: ['wrapKey', 'unwrapKey'] },
            'HMAC': { secret: ['sign', 'verify'] },
            'RSA-PSS': { public: ['verify'], private: ['sign'] },
            'RSASSA-PKCS1-V1_5': { public: ['verify'], private: ['sign'] },
            'RSA-OAEP': { public: ['encrypt', 'wrapKey'], private: ['decrypt', 'unwrapKey'] },
            'ECDSA': { public: ['verify'], private: ['sign'] },
            'ECDH': { public: [], private: ['deriveKey', 'deriveBits'] },
            'HKDF': { secret: ['deriveKey', 'deriveBits'] },
            'PBKDF2': { secret: ['deriveKey', 'deriveBits'] },
        };
        const allowed = (usageTable[algorithm] && usageTable[algorithm][keyType]) || [];
        for (const usage of usages || []) {
            if (!allowed.includes(usage)) {
                throw new Error(`Usage ${usage} is not allowed for ${algorithm}`);
            }
        }
    }

    function splitUsages(algorithm, usages) {
        const usageTable = {
            'RSA-PSS': { public: ['verify'], private: ['sign'] },
            'RSASSA-PKCS1-V1_5': { public: ['verify'], private: ['sign'] },
            'RSA-OAEP': { public: ['encrypt', 'wrapKey'], private: ['decrypt', 'unwrapKey'] },
            'ECDSA': { public: ['verify'], private: ['sign'] },
            'ECDH': { public: [], private: ['deriveKey', 'deriveBits'] },
        };
        const allowed = usageTable[algorithm] || { public: [], private: [] };
        const publicUsages = [];
        const privateUsages = [];
        for (const usage of usages || []) {
            if (allowed.public.includes(usage)) {
                publicUsages.push(usage);
            } else if (allowed.private.includes(usage)) {
                privateUsages.push(usage);
            } else {
                throw new Error(`Usage ${usage} is not allowed for ${algorithm}`);
            }
        }
        return { publicUsages, privateUsages };
    }

    function assertKeyAlgorithm(key, algorithm) {
        const keyAlg = normalizeAlgorithmName(key.algorithm || {});
        if (keyAlg && algorithm && keyAlg !== normalizeAlgorithmName(algorithm)) {
            throw new Error('Algorithm mismatch');
        }
    }

    function ensureKeyType(key, allowed, label) {
        if (!allowed.includes(key.type)) {
            throw new Error(`${label} requires ${allowed.join(' or ')} key`);
        }
    }

    function jwkAlgorithmName(key) {
        const name = normalizeAlgorithmName(key.algorithm || {});
        if (name.startsWith('RSA')) {
            return 'RSA';
        }
        if (name === 'ECDSA' || name === 'ECDH') {
            return 'EC';
        }
        return name;
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

    function parsePublicExponent(value) {
        if (value === undefined || value === null) {
            return 65537;
        }
        if (typeof value === 'number') {
            return value;
        }
        const bytes = toBuffer(value);
        let result = 0;
        for (const byte of bytes) {
            result = (result << 8) + byte;
        }
        return result || 65537;
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
            assertKeyAlgorithm(key, algorithm);
            if (key.algorithm && key.algorithm.name === 'HMAC') {
                ensureKeyType(key, ['secret'], 'HMAC sign');
                const hash = resolveHashName(key.algorithm.hash);
                const hmac = globalThis.crypto.createHmac(hash, key[kKeyData]);
                hmac.update(toBuffer(data));
                return toArrayBuffer(hmac.digest());
            }
            ensureKeyType(key, ['private'], 'sign');
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
            assertKeyAlgorithm(key, algorithm);
            if (key.algorithm && key.algorithm.name === 'HMAC') {
                ensureKeyType(key, ['secret'], 'HMAC verify');
                const hash = resolveHashName(key.algorithm.hash);
                const hmac = globalThis.crypto.createHmac(hash, key[kKeyData]);
                hmac.update(toBuffer(data));
                const digest = hmac.digest();
                return globalThis.crypto.timingSafeEqual(digest, toBuffer(signature));
            }
            ensureKeyType(key, ['public'], 'verify');
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
            const name = normalizeAlgorithmName(algorithm);
            assertKeyAlgorithm(key, algorithm);
            if (name === 'AES-GCM') {
                ensureKeyType(key, ['secret'], 'AES-GCM encrypt');
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
            }
            if (name === 'AES-CBC') {
                ensureKeyType(key, ['secret'], 'AES-CBC encrypt');
                const result = _cryptoSubtleEncryptAesCbc(
                    key[kKeyData],
                    toBuffer(data),
                    { iv: toBuffer(algorithm.iv) }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            if (name === 'AES-CTR') {
                ensureKeyType(key, ['secret'], 'AES-CTR encrypt');
                const result = _cryptoSubtleEncryptAesCtr(
                    key[kKeyData],
                    toBuffer(data),
                    { counter: toBuffer(algorithm.counter), length: algorithm.length }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            if (name === 'RSA-OAEP') {
                ensureKeyType(key, ['public'], 'RSA-OAEP encrypt');
                const result = _cryptoSubtleRsaOaepEncrypt(
                    { key: key[kKeyData], format: key[kKeyFormat], type: key[kKeyType] },
                    toBuffer(data),
                    { hash: resolveHashName(algorithm.hash), label: algorithm.label ? toBuffer(algorithm.label) : undefined }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            throw new Error(`Unsupported algorithm ${name}`);
        },
        async decrypt(algorithm, key, data) {
            const name = normalizeAlgorithmName(algorithm);
            assertKeyAlgorithm(key, algorithm);
            if (name === 'AES-GCM') {
                ensureKeyType(key, ['secret'], 'AES-GCM decrypt');
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
            }
            if (name === 'AES-CBC') {
                ensureKeyType(key, ['secret'], 'AES-CBC decrypt');
                const result = _cryptoSubtleDecryptAesCbc(
                    key[kKeyData],
                    toBuffer(data),
                    { iv: toBuffer(algorithm.iv) }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            if (name === 'AES-CTR') {
                ensureKeyType(key, ['secret'], 'AES-CTR decrypt');
                const result = _cryptoSubtleDecryptAesCtr(
                    key[kKeyData],
                    toBuffer(data),
                    { counter: toBuffer(algorithm.counter), length: algorithm.length }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            if (name === 'RSA-OAEP') {
                ensureKeyType(key, ['private'], 'RSA-OAEP decrypt');
                const result = _cryptoSubtleRsaOaepDecrypt(
                    { key: key[kKeyData], format: key[kKeyFormat], type: key[kKeyType] },
                    toBuffer(data),
                    { hash: resolveHashName(algorithm.hash), label: algorithm.label ? toBuffer(algorithm.label) : undefined }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            throw new Error(`Unsupported algorithm ${name}`);
        },
        async generateKey(algorithm, extractable, usages) {
            const name = normalizeAlgorithmName(algorithm);
            if (name === 'AES-GCM' || name === 'AES-CBC' || name === 'AES-CTR' || name === 'AES-KW') {
                const length = algorithm.length || 256;
                const bytes = globalThis.crypto.randomBytes(Math.ceil(length / 8));
                ensureUsage(name, 'secret', usages);
                return createCryptoKey('secret', { name, length }, extractable, usages, bytes, 'raw', 'raw');
            }
            if (name === 'HMAC') {
                const length = algorithm.length || 256;
                const bytes = globalThis.crypto.randomBytes(Math.ceil(length / 8));
                ensureUsage(name, 'secret', usages);
                return createCryptoKey('secret', { name, length, hash: algorithm.hash }, extractable, usages, bytes, 'raw', 'raw');
            }
            if (name === 'RSA-PSS' || name === 'RSASSA-PKCS1-V1_5' || name === 'RSA-OAEP') {
                const result = await _cryptoGenerateKeyPair('rsa', {
                    modulusLength: algorithm.modulusLength || 2048,
                    publicExponent: parsePublicExponent(algorithm.publicExponent),
                    publicKeyEncoding: { format: 'der', type: 'spki' },
                    privateKeyEncoding: { format: 'der', type: 'pkcs8' },
                });
                const { publicUsages, privateUsages } = splitUsages(name, usages);
                const publicKey = createCryptoKey(
                    'public',
                    { name, hash: algorithm.hash },
                    true,
                    publicUsages,
                    normalizeKeyOutput(result.publicKey),
                    'der',
                    'spki'
                );
                const privateKey = createCryptoKey(
                    'private',
                    { name, hash: algorithm.hash },
                    extractable,
                    privateUsages,
                    normalizeKeyOutput(result.privateKey),
                    'der',
                    'pkcs8'
                );
                return { publicKey, privateKey };
            }
            if (name === 'ECDSA' || name === 'ECDH') {
                const result = await _cryptoGenerateKeyPair('ec', {
                    namedCurve: algorithm.namedCurve || 'prime256v1',
                    publicKeyEncoding: { format: 'der', type: 'spki' },
                    privateKeyEncoding: { format: 'der', type: 'pkcs8' },
                });
                const { publicUsages, privateUsages } = splitUsages(name, usages);
                const publicKey = createCryptoKey(
                    'public',
                    { name, namedCurve: algorithm.namedCurve || 'prime256v1' },
                    true,
                    publicUsages,
                    normalizeKeyOutput(result.publicKey),
                    'der',
                    'spki'
                );
                const privateKey = createCryptoKey(
                    'private',
                    { name, namedCurve: algorithm.namedCurve || 'prime256v1' },
                    extractable,
                    privateUsages,
                    normalizeKeyOutput(result.privateKey),
                    'der',
                    'pkcs8'
                );
                return { publicKey, privateKey };
            }
            throw new Error(`Unsupported algorithm ${name}`);
        },
        async importKey(format, keyData, algorithm, extractable, usages) {
            const name = normalizeAlgorithmName(algorithm);
            if (format === 'raw') {
                ensureUsage(name, 'secret', usages);
                return createCryptoKey('secret', { name, hash: algorithm.hash, length: algorithm.length }, extractable, usages, toBuffer(keyData), 'raw', 'raw');
            }
            if (format === 'pkcs8') {
                ensureUsage(name, 'private', usages);
                return createCryptoKey('private', { name, hash: algorithm.hash, namedCurve: algorithm.namedCurve }, extractable, usages, toBuffer(keyData), 'der', 'pkcs8');
            }
            if (format === 'spki') {
                ensureUsage(name, 'public', usages);
                return createCryptoKey('public', { name, hash: algorithm.hash, namedCurve: algorithm.namedCurve }, true, usages, toBuffer(keyData), 'der', 'spki');
            }
            if (format === 'jwk') {
                const jwk = typeof keyData === 'string' ? JSON.parse(keyData) : keyData;
                if (jwk.ext === false && extractable) {
                    throw new Error('Key is not extractable');
                }
                if (Array.isArray(jwk.key_ops)) {
                    for (const usage of usages || []) {
                        if (!jwk.key_ops.includes(usage)) {
                            throw new Error('Key usage not allowed by jwk.key_ops');
                        }
                    }
                }
                if (jwk.kty && jwk.kty.toUpperCase() === 'OCT') {
                    const bytes = base64UrlDecode(jwk.k || '');
                    ensureUsage(name, 'secret', usages);
                    return createCryptoKey('secret', { name, hash: algorithm.hash, length: bytes.length * 8 }, extractable, usages, bytes, 'raw', 'raw');
                }
                const material = _cryptoJwkToDer(jwk);
                const keyType = material.keyType;
                const kind = keyType === 'spki' ? 'public' : 'private';
                ensureUsage(name, kind, usages);
                return createCryptoKey(kind, { name, hash: algorithm.hash, namedCurve: algorithm.namedCurve }, extractable, usages, normalizeKeyOutput(material), 'der', keyType);
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
            if (format === 'jwk') {
                if (key.type === 'secret') {
                    return {
                        kty: 'oct',
                        k: base64UrlEncode(key[kKeyData]),
                        ext: true,
                        key_ops: key.usages.slice(),
                    };
                }
                const jwk = _cryptoDerToJwk(
                    jwkAlgorithmName(key),
                    key[kKeyType],
                    key[kKeyData]
                );
                jwk.ext = true;
                jwk.key_ops = key.usages.slice();
                return jwk;
            }
            throw new Error(`Unsupported export format ${format}`);
        },
        async deriveBits(algorithm, baseKey, length) {
            if (!baseKey || !(baseKey instanceof CryptoKey)) {
                throw new Error('Invalid CryptoKey');
            }
            const name = normalizeAlgorithmName(algorithm);
            if (name === 'ECDH') {
                ensureKeyType(baseKey, ['private'], 'ECDH deriveBits');
                const publicKey = algorithm.public;
                ensureKeyType(publicKey, ['public'], 'ECDH public key');
                const result = _cryptoSubtleDeriveBitsEcdh(
                    { key: baseKey[kKeyData], format: baseKey[kKeyFormat], type: baseKey[kKeyType] },
                    { key: publicKey[kKeyData], format: publicKey[kKeyFormat], type: publicKey[kKeyType] }
                );
                const raw = fromBufferResult(result);
                const byteLen = Math.ceil((length || raw.length * 8) / 8);
                return toArrayBuffer(raw.slice(0, byteLen));
            }
            if (name === 'HKDF') {
                ensureKeyType(baseKey, ['secret'], 'HKDF deriveBits');
                const result = _cryptoSubtleDeriveBitsHkdf(
                    baseKey[kKeyData],
                    {
                        hash: resolveHashName(algorithm.hash),
                        salt: toBuffer(algorithm.salt),
                        info: toBuffer(algorithm.info || new Uint8Array(0)),
                        length: length || (algorithm.length || 256),
                    }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            if (name === 'PBKDF2') {
                ensureKeyType(baseKey, ['secret'], 'PBKDF2 deriveBits');
                const result = _cryptoSubtleDeriveBitsPbkdf2(
                    baseKey[kKeyData],
                    {
                        hash: resolveHashName(algorithm.hash),
                        salt: toBuffer(algorithm.salt),
                        iterations: algorithm.iterations,
                        length: length || (algorithm.length || 256),
                    }
                );
                return toArrayBuffer(fromBufferResult(result));
            }
            throw new Error(`Unsupported algorithm ${name}`);
        },
        async deriveKey(algorithm, baseKey, derivedKeyType, extractable, usages) {
            const bits = await subtle.deriveBits(algorithm, baseKey, derivedKeyType.length || 256);
            return subtle.importKey('raw', bits, derivedKeyType, extractable, usages);
        },
        async wrapKey(format, key, wrappingKey, wrapAlgorithm) {
            const exported = await subtle.exportKey(format, key);
            const data = format === 'jwk' ? Buffer.from(JSON.stringify(exported)) : Buffer.from(exported);
            const name = normalizeAlgorithmName(wrapAlgorithm);
            if (name === 'AES-KW') {
                const wrapped = _cryptoSubtleWrapAesKw(wrappingKey[kKeyData], data);
                return toArrayBuffer(fromBufferResult(wrapped));
            }
            return subtle.encrypt(wrapAlgorithm, wrappingKey, data);
        },
        async unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, usages) {
            const name = normalizeAlgorithmName(unwrapAlgorithm);
            const plaintext = name === 'AES-KW'
                ? toArrayBuffer(fromBufferResult(_cryptoSubtleUnwrapAesKw(unwrappingKey[kKeyData], toBuffer(wrappedKey))))
                : await subtle.decrypt(unwrapAlgorithm, unwrappingKey, wrappedKey);
            const data = format === 'jwk' ? JSON.parse(Buffer.from(plaintext).toString('utf8')) : plaintext;
            return subtle.importKey(format, data, unwrappedKeyAlgorithm, extractable, usages);
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
