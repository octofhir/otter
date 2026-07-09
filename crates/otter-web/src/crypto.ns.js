// The JS half of the native `crypto` namespace: WebIDL validation
// with exact DOMException names over the native compute members,
// plus the SubtleCrypto shape. Consumes and deletes the private
// `__nativeRandomFill` / `__nativeDigest` members installed by the
// declaration, so no hidden hooks remain reachable.
(function () {
  'use strict';
  const nativeRandomFill = crypto.__nativeRandomFill;
  const nativeDigest = crypto.__nativeDigest;
  delete crypto.__nativeRandomFill;
  delete crypto.__nativeDigest;

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

  Object.defineProperty(crypto, 'getRandomValues', {
    value: function getRandomValues(array) {
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
    },
    writable: true,
    enumerable: false,
    configurable: true,
  });

  const DIGEST_ALGORITHMS = new Set(['SHA-1', 'SHA-256', 'SHA-384', 'SHA-512']);

  class SubtleCrypto {
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
  Object.defineProperty(SubtleCrypto.prototype, Symbol.toStringTag, {
    value: 'SubtleCrypto',
    writable: false,
    enumerable: false,
    configurable: true,
  });

  const subtle = new SubtleCrypto();
  Object.defineProperty(crypto, 'subtle', {
    get() { return subtle; },
    enumerable: false,
    configurable: true,
  });
})();
