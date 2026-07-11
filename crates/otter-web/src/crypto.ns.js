// The JS half of the native `crypto` namespace: WebIDL validation
// with exact DOMException names over the native compute members,
// plus the `Crypto` / `SubtleCrypto` / `CryptoKey` class globals.
// Consumes and deletes the private `__nativeRandomFill` /
// `__nativeDigest` members installed by the declaration, so no hidden
// hooks remain reachable.
//
// The three interfaces have no constructor (`new Crypto()` etc. throw
// "Illegal constructor", matching WebCrypto §[Exposed] classes without
// [Constructor]). The `crypto` singleton is reparented onto
// `Crypto.prototype` in place — same object identity — so its methods
// live on the prototype and `crypto instanceof Crypto` holds.
(function () {
  'use strict';
  const nativeRandomFill = crypto.__nativeRandomFill;
  const nativeDigest = crypto.__nativeDigest;
  // `randomUUID` is installed as a native own-member on the namespace
  // object; capture it before reparenting so it can move onto the
  // prototype.
  const nativeRandomUUID = crypto.randomUUID;
  delete crypto.__nativeRandomFill;
  delete crypto.__nativeDigest;

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

  class CryptoKey {
    constructor() { throw new TypeError('Illegal constructor'); }
  }
  tagged(CryptoKey.prototype, 'CryptoKey');

  class SubtleCrypto {
    constructor() { throw new TypeError('Illegal constructor'); }
    async digest(algorithm, data) {
      let name = algorithm !== null && typeof algorithm === 'object'
        ? algorithm.name
        : algorithm;
      name = String(name).toUpperCase();
      if (!DIGEST_ALGORITHMS.has(name)) {
        throw new DOMException(
          `Unrecognized algorithm name: ${name}`,
          'NotSupportedError',
        );
      }
      if (!(data instanceof ArrayBuffer) && !ArrayBuffer.isView(data)) {
        throw new TypeError('data must be an ArrayBuffer or ArrayBufferView');
      }
      return nativeDigest(name, data);
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
})();
