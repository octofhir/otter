'use strict';
// `internal/util/types`, self-contained. The public `util` module re-exports
// this object, so nothing here may require `util` back. The checks combine
// brand tags with instanceof; `Object.prototype.toString` still answers the
// internal class for the tagged builtins.

const toString = (value) => Object.prototype.toString.call(value).slice(8, -1);

const typedArrayTags = new Set([
  'Int8Array', 'Uint8Array', 'Uint8ClampedArray', 'Int16Array', 'Uint16Array',
  'Int32Array', 'Uint32Array', 'Float32Array', 'Float64Array',
  'BigInt64Array', 'BigUint64Array',
]);

function taggedCheck(tag) {
  return (value) => toString(value) === tag;
}

// Brand checks: a lie in Symbol.toStringTag or a swapped prototype must not
// fool these, so each probes an internal-slot-requiring intrinsic.
function brandCheck(fn) {
  return (value) => {
    if (typeof value !== 'object' || value === null) return false;
    try {
      fn.call(value);
      return true;
    } catch {
      return false;
    }
  };
}
const dateBrand = brandCheck(Date.prototype.getTime);
const stringBrand = brandCheck(String.prototype.valueOf);
const numberBrand = brandCheck(Number.prototype.valueOf);
const booleanBrand = brandCheck(Boolean.prototype.valueOf);
const symbolBrand = brandCheck(Symbol.prototype.valueOf);
const bigintBrand = brandCheck(BigInt.prototype.valueOf);
const mapBrand = brandCheck(Object.getOwnPropertyDescriptor(Map.prototype, 'size').get);
const setBrand = brandCheck(Object.getOwnPropertyDescriptor(Set.prototype, 'size').get);
const regexpBrand = (value) => {
  if (typeof value !== 'object' || value === null) return false;
  try {
    RegExp.prototype.exec.call(value, '');
    return true;
  } catch {
    return false;
  }
};

const AsyncFunctionCtor = Object.getPrototypeOf(async function () {}).constructor;
const GeneratorFunctionCtor = Object.getPrototypeOf(function* () {}).constructor;
const AsyncGeneratorFunctionCtor = Object.getPrototypeOf(async function* () {}).constructor;

const types = {
  isArgumentsObject: taggedCheck('Arguments'),
  isArrayBuffer: (value) => value instanceof ArrayBuffer,
  isSharedArrayBuffer: (value) =>
    typeof SharedArrayBuffer === 'function' && value instanceof SharedArrayBuffer,
  isAnyArrayBuffer: (value) => types.isArrayBuffer(value) || types.isSharedArrayBuffer(value),
  isArrayBufferView: ArrayBuffer.isView,
  isDataView: (value) => value instanceof DataView,
  isTypedArray: (value) => ArrayBuffer.isView(value) && !(value instanceof DataView),
  isUint8Array: (value) => value instanceof Uint8Array,
  isDate: dateBrand,
  isRegExp: regexpBrand,
  isMap: mapBrand,
  isSet: setBrand,
  isWeakMap: (value) => value instanceof WeakMap,
  isWeakSet: (value) => value instanceof WeakSet,
  isPromise: (value) => value instanceof Promise,
  isProxy: () => false,
  isExternal: () => false,
  isModuleNamespaceObject: taggedCheck('Module'),
  // Real brand: an [[ErrorData]] slot check, not a prototype walk — a plain
  // object with Error.prototype behind it must answer false.
  isNativeError: (value) => Error.isError(value),
  isMapIterator: taggedCheck('Map Iterator'),
  isSetIterator: taggedCheck('Set Iterator'),
  isGeneratorFunction: (value) =>
    typeof value === 'function' && value instanceof GeneratorFunctionCtor &&
    !(value instanceof AsyncGeneratorFunctionCtor),
  isAsyncGeneratorFunction: (value) =>
    typeof value === 'function' && value instanceof AsyncGeneratorFunctionCtor,
  isAsyncFunction: (value) =>
    typeof value === 'function' &&
    (value instanceof AsyncFunctionCtor || value instanceof AsyncGeneratorFunctionCtor),
  isGeneratorObject: taggedCheck('Generator'),
  isNumberObject: numberBrand,
  isStringObject: stringBrand,
  isBooleanObject: booleanBrand,
  isSymbolObject: symbolBrand,
  isBigIntObject: bigintBrand,
  isBoxedPrimitive: (value) =>
    typeof value === 'object' && value !== null &&
    (types.isNumberObject(value) || types.isStringObject(value) ||
     types.isBooleanObject(value) || types.isSymbolObject(value) ||
     types.isBigIntObject(value)),
  isBlob: (value) => typeof Blob === 'function' && value instanceof Blob,
  isKeyObject: () => false,
  isCryptoKey: () => false,
};

for (const tag of typedArrayTags) {
  const Ctor = globalThis[tag];
  types[`is${tag}`] = Ctor
    ? (value) => value instanceof Ctor
    : () => false;
}
// The Uint8Array narrowing above stays the instanceof form.
types.isUint8Array = (value) => value instanceof Uint8Array;
types.isFloat16Array = typeof Float16Array === 'function'
  ? (value) => value instanceof Float16Array
  : () => false;

module.exports = types;
