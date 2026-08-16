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
  isDate: (value) => value instanceof Date,
  isRegExp: (value) => value instanceof RegExp,
  isMap: (value) => value instanceof Map,
  isSet: (value) => value instanceof Set,
  isWeakMap: (value) => value instanceof WeakMap,
  isWeakSet: (value) => value instanceof WeakSet,
  isPromise: (value) => value instanceof Promise,
  isProxy: () => false,
  isExternal: () => false,
  isModuleNamespaceObject: taggedCheck('Module'),
  isNativeError: (value) => value instanceof Error,
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
  isNumberObject: taggedCheck('Number'),
  isStringObject: taggedCheck('String'),
  isBooleanObject: taggedCheck('Boolean'),
  isSymbolObject: taggedCheck('Symbol'),
  isBigIntObject: taggedCheck('BigInt'),
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
