// The JS half of the native `crypto` namespace: WebIDL validation
// with exact DOMException names over the native compute members,
// plus the `Crypto` / `SubtleCrypto` / `CryptoKey` class globals.
// Runs as a `#[js_namespace]` factory glue: the `__`-prefixed native
// compute members are handed in through the `natives` bag (see the
// macro), never left on the public `crypto` object, so this file just
// reads `natives.<name>` — no hidden hooks to poke or delete by hand.
//
// The three interfaces have no constructor (`new Crypto()` etc. throw
// "Illegal constructor", matching WebCrypto §[Exposed] classes without
// [Constructor]). The `crypto` singleton is reparented onto
// `Crypto.prototype` in place — same object identity — so its methods
// live on the prototype and `crypto instanceof Crypto` holds.
  // `natives` is the private compute bag the `#[js_namespace]` factory hands to
  // this glue (see the macro): the `__`-prefixed members were moved off the
  // public `crypto` object into it, keyed without the prefix. `randomUUID` is a
  // public member and stays on `crypto`.
  const nativeRandomFill = natives.nativeRandomFill;
  const nativeDigest = natives.nativeDigest;
  const nativeHmacSign = natives.hmacSign;
  const nativePbkdf2 = natives.pbkdf2;
  const nativeAesGcmEncrypt = natives.aesGcmEncrypt;
  const nativeAesGcmDecrypt = natives.aesGcmDecrypt;
  const nativeRandomUUID = crypto.randomUUID;

  function tagged(proto, tag) {
    Object.defineProperty(proto, Symbol.toStringTag, {
      value: tag,
      writable: false,
      enumerable: false,
      configurable: true,
    });
  }

  function def(name, value) {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  // Drop an own property that has been re-homed onto a prototype. A
  // non-configurable native member survives as a harmless shadow of the
  // identical prototype method.
  function reHome(obj, key) {
    try { delete obj[key]; } catch (_) { /* non-configurable: keep shadow */ }
  }

  const INTEGER_VIEW_CTORS = [
    Int8Array, Uint8Array, Uint8ClampedArray,
    Int16Array, Uint16Array, Int32Array, Uint32Array,
    typeof BigInt64Array === 'function' ? BigInt64Array : null,
    typeof BigUint64Array === 'function' ? BigUint64Array : null,
  ].filter((ctor) => ctor !== null);

  function isIntegerView(view) {
    for (const ctor of INTEGER_VIEW_CTORS) {
      if (view instanceof ctor) return true;
    }
    return false;
  }

  const DIGEST_ALGORITHMS = new Set(['SHA-1', 'SHA-256', 'SHA-384', 'SHA-512']);

  // --- algorithm / data helpers ---
  function normalizeAlgorithm(algorithm) {
    const dict = typeof algorithm === 'string' ? { name: algorithm } : (algorithm || {});
    if (dict.name === undefined) throw new TypeError('algorithm must have a name');
    return Object.assign({}, dict, { name: String(dict.name).toUpperCase() });
  }
  function normalizeHash(hash) {
    const name = typeof hash === 'string' ? hash : (hash && hash.name);
    const upper = String(name).toUpperCase();
    if (!DIGEST_ALGORITHMS.has(upper)) {
      throw new DOMException(`Unrecognized hash: ${name}`, 'NotSupportedError');
    }
    return upper;
  }
  function requireBufferSource(value, label) {
    if (!(value instanceof ArrayBuffer) && !ArrayBuffer.isView(value)) {
      throw new TypeError(`${label} must be an ArrayBuffer or ArrayBufferView`);
    }
    return value;
  }
  function bytesEqual(a, b) {
    if (a.byteLength !== b.byteLength) return false;
    let diff = 0;
    for (let i = 0; i < a.byteLength; i++) diff |= a[i] ^ b[i];
    return diff === 0;
  }

  // Private CryptoKey state (raw material never leaves except via exportKey).
  const kMaterial = Symbol('material');
  const kAlgorithm = Symbol('algorithm');
  const kType = Symbol('type');
  const kExtractable = Symbol('extractable');
  const kUsages = Symbol('usages');

  class CryptoKey {
    constructor() { throw new TypeError('Illegal constructor'); }
    get type() { return this[kType]; }
    get extractable() { return this[kExtractable]; }
    get algorithm() { return this[kAlgorithm]; }
    get usages() { return this[kUsages].slice(); }
  }
  tagged(CryptoKey.prototype, 'CryptoKey');

  function makeKey(material, algorithm, type, extractable, usages) {
    const key = Object.create(CryptoKey.prototype);
    key[kMaterial] = new Uint8Array(material);
    key[kAlgorithm] = algorithm;
    key[kType] = type;
    key[kExtractable] = Boolean(extractable);
    key[kUsages] = usages.slice();
    return key;
  }

  class SubtleCrypto {
    constructor() { throw new TypeError('Illegal constructor'); }

    async digest(algorithm, data) {
      const name = normalizeAlgorithm(algorithm).name;
      if (!DIGEST_ALGORITHMS.has(name)) {
        throw new DOMException(`Unrecognized algorithm name: ${name}`, 'NotSupportedError');
      }
      return nativeDigest(name, requireBufferSource(data, 'data'));
    }

    async generateKey(algorithm, extractable, usages) {
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name === 'HMAC') {
        const hash = normalizeHash(algo.hash);
        const blockBytes = hash === 'SHA-384' || hash === 'SHA-512' ? 128 : 64;
        const length = algo.length !== undefined ? Math.ceil(Number(algo.length) / 8) : blockBytes;
        const material = crypto.getRandomValues(new Uint8Array(length));
        return makeKey(material, { name: 'HMAC', hash: { name: hash }, length: length * 8 },
          'secret', extractable, usages);
      }
      if (algo.name === 'AES-GCM') {
        const length = Number(algo.length);
        if (length !== 128 && length !== 256) {
          throw new DOMException('AES-GCM key length must be 128 or 256', 'OperationError');
        }
        const material = crypto.getRandomValues(new Uint8Array(length / 8));
        return makeKey(material, { name: 'AES-GCM', length }, 'secret', extractable, usages);
      }
      throw new DOMException(`generateKey: unsupported algorithm ${algo.name}`, 'NotSupportedError');
    }

    async importKey(format, keyData, algorithm, extractable, usages) {
      if (format !== 'raw') {
        throw new DOMException(`importKey: unsupported format ${format}`, 'NotSupportedError');
      }
      const bytes = new Uint8Array(
        requireBufferSource(keyData, 'keyData') instanceof ArrayBuffer
          ? keyData.slice(0)
          : keyData.buffer.slice(keyData.byteOffset, keyData.byteOffset + keyData.byteLength),
      );
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name === 'HMAC') {
        const hash = normalizeHash(algo.hash);
        return makeKey(bytes, { name: 'HMAC', hash: { name: hash }, length: bytes.length * 8 },
          'secret', extractable, usages);
      }
      if (algo.name === 'AES-GCM') {
        return makeKey(bytes, { name: 'AES-GCM', length: bytes.length * 8 }, 'secret', extractable, usages);
      }
      if (algo.name === 'PBKDF2') {
        return makeKey(bytes, { name: 'PBKDF2' }, 'secret', false, usages);
      }
      throw new DOMException(`importKey: unsupported algorithm ${algo.name}`, 'NotSupportedError');
    }

    async exportKey(format, key) {
      if (format !== 'raw') {
        throw new DOMException(`exportKey: unsupported format ${format}`, 'NotSupportedError');
      }
      if (!(key instanceof CryptoKey)) throw new TypeError('key must be a CryptoKey');
      if (!key[kExtractable]) throw new DOMException('key is not extractable', 'InvalidAccessError');
      return key[kMaterial].buffer.slice(
        key[kMaterial].byteOffset,
        key[kMaterial].byteOffset + key[kMaterial].byteLength,
      );
    }

    async sign(algorithm, key, data) {
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name !== 'HMAC') {
        throw new DOMException(`sign: unsupported algorithm ${algo.name}`, 'NotSupportedError');
      }
      const hash = normalizeHash(key[kAlgorithm].hash);
      return nativeHmacSign(hash, key[kMaterial], requireBufferSource(data, 'data'));
    }

    async verify(algorithm, key, signature, data) {
      const expected = new Uint8Array(await this.sign(algorithm, key, data));
      const provided = new Uint8Array(
        signature instanceof ArrayBuffer ? signature
          : requireBufferSource(signature, 'signature').buffer.slice(
            signature.byteOffset, signature.byteOffset + signature.byteLength),
      );
      return bytesEqual(expected, provided);
    }

    async encrypt(algorithm, key, data) {
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name !== 'AES-GCM') {
        throw new DOMException(`encrypt: unsupported algorithm ${algo.name}`, 'NotSupportedError');
      }
      const aad = algo.additionalData !== undefined
        ? requireBufferSource(algo.additionalData, 'additionalData') : new Uint8Array(0);
      return nativeAesGcmEncrypt(key[kMaterial], requireBufferSource(algo.iv, 'iv'), aad,
        requireBufferSource(data, 'data'));
    }

    async decrypt(algorithm, key, data) {
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name !== 'AES-GCM') {
        throw new DOMException(`decrypt: unsupported algorithm ${algo.name}`, 'NotSupportedError');
      }
      const aad = algo.additionalData !== undefined
        ? requireBufferSource(algo.additionalData, 'additionalData') : new Uint8Array(0);
      try {
        return await nativeAesGcmDecrypt(key[kMaterial], requireBufferSource(algo.iv, 'iv'), aad,
          requireBufferSource(data, 'data'));
      } catch (err) {
        throw new DOMException(String(err && err.message || err), 'OperationError');
      }
    }

    async deriveBits(algorithm, baseKey, length) {
      const algo = normalizeAlgorithm(algorithm);
      if (algo.name !== 'PBKDF2') {
        throw new DOMException(`deriveBits: unsupported algorithm ${algo.name}`, 'NotSupportedError');
      }
      const hash = normalizeHash(algo.hash);
      const iterations = Number(algo.iterations);
      if (!(iterations > 0)) throw new DOMException('PBKDF2 iterations must be positive', 'OperationError');
      return nativePbkdf2(hash, baseKey[kMaterial], requireBufferSource(algo.salt, 'salt'),
        iterations, Math.ceil(Number(length) / 8));
    }

    async deriveKey(algorithm, baseKey, derivedKeyType, extractable, usages) {
      const derived = normalizeAlgorithm(derivedKeyType);
      const lengthBits = derived.name === 'AES-GCM' ? Number(derived.length)
        : derived.name === 'HMAC' ? (derived.length !== undefined ? Number(derived.length)
          : (normalizeHash(derived.hash) === 'SHA-384' || normalizeHash(derived.hash) === 'SHA-512' ? 1024 : 512))
        : 0;
      if (lengthBits === 0) {
        throw new DOMException(`deriveKey: unsupported derived type ${derived.name}`, 'NotSupportedError');
      }
      const bits = await this.deriveBits(algorithm, baseKey, lengthBits);
      return this.importKey('raw', bits, derivedKeyType, extractable, usages);
    }
  }
  tagged(SubtleCrypto.prototype, 'SubtleCrypto');
  const subtle = Object.create(SubtleCrypto.prototype);

  class Crypto {
    constructor() { throw new TypeError('Illegal constructor'); }
    getRandomValues(array) {
      if (!isIntegerView(array)) {
        throw new DOMException(
          'getRandomValues requires an integer TypedArray',
          'TypeMismatchError',
        );
      }
      if (array.byteLength > 65536) {
        throw new DOMException(
          `getRandomValues length (${array.byteLength} bytes) exceeds the 65536-byte quota`,
          'QuotaExceededError',
        );
      }
      return nativeRandomFill(array);
    }
    randomUUID() { return nativeRandomUUID(); }
    get subtle() { return subtle; }
  }
  tagged(Crypto.prototype, 'Crypto');

  // Reparent the existing `crypto` namespace object onto `Crypto.prototype`
  // (same identity) and re-home its members so the prototype methods surface.
  Object.setPrototypeOf(crypto, Crypto.prototype);
  reHome(crypto, 'getRandomValues');
  reHome(crypto, 'randomUUID');
  reHome(crypto, 'subtle');

  def('Crypto', Crypto);
  def('SubtleCrypto', SubtleCrypto);
  def('CryptoKey', CryptoKey);
