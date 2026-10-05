// The JS half of the native `WebAssembly` namespace. Runs as a
// `#[js_namespace]` factory glue: `__ns` is the `WebAssembly` namespace
// object and `natives` is the private compute bag (here it holds
// `buildInstance`, `errorClasses` and `jsTag`, moved off the public
// object by the macro).
//
// This layer owns the pieces that are cleaner in JS: relocating the
// native reference-type constructors onto the namespace, the
// original intrinsic `CompileError` / `LinkError` / `RuntimeError` classes, the
// synchronous `Instance` class (which delegates to the native
// `buildInstance`), the namespace brand,
// and the streaming forms that read a `Response` body before delegating
// to the native `compile` / `instantiate`.

// The native reference-type classes install as hidden global properties
// keyed by their dotted class name; move each onto `WebAssembly` and give
// the constructor its short WebIDL name.
function relocate(shortName) {
  const ctor = globalThis['WebAssembly.' + shortName];
  if (typeof ctor === 'function') {
    __ns[shortName] = ctor;
    try {
      Object.defineProperty(ctor, 'name', { value: shortName, configurable: true });
    } catch (_) { /* name already locked: leave it */ }
  }
}
relocate('Module');
relocate('Memory');
relocate('Global');
relocate('Table');
relocate('Tag');
relocate('Exception');

// The constructors and prototypes are the original traced realm intrinsics.
// Engine failures never read these replaceable namespace properties.
const errorClasses = natives.errorClasses();
__ns.CompileError = errorClasses.CompileError;
__ns.LinkError = errorClasses.LinkError;
__ns.RuntimeError = errorClasses.RuntimeError;

// Synchronous native instantiation preserves actual classes and user throws.
const Instance = class Instance {
  constructor(module, importObject) {
    return natives.buildInstance(module, importObject);
  }
};
Object.defineProperty(Instance.prototype, Symbol.toStringTag, {
  value: 'WebAssembly.Instance',
  writable: false,
  enumerable: false,
  configurable: true,
});
__ns.Instance = Instance;
// The native instantiate paths reparent their instance objects onto this
// prototype; the class-constructor value's `.prototype` is not reachable
// through the marshalling layer, so mirror it as a hidden object property.
Object.defineProperty(__ns, '__instanceProto', {
  value: Instance.prototype,
  writable: false,
  enumerable: false,
  configurable: true,
});

// `WebAssembly.JSTag` is the realm-wide well-known tag (parameters:
// `[externref]`) that carries a JS value across wasm frames. It is a readonly
// `WebAssembly.Tag` instance built once by the native factory.
Object.defineProperty(__ns, 'JSTag', {
  value: natives.jsTag(),
  writable: false,
  enumerable: false,
  configurable: true,
});

// Streaming: read the (possibly promised) Response's bytes, then hand off
// to the native compile/instantiate.
async function sourceBytes(source) {
  const response = await source;
  if (!response || typeof response.arrayBuffer !== 'function') {
    throw new TypeError('WebAssembly streaming source must be a Response');
  }
  return response.arrayBuffer();
}
__ns.compileStreaming = async function compileStreaming(source) {
  return __ns.compile(await sourceBytes(source));
};
__ns.instantiateStreaming = async function instantiateStreaming(source, importObject) {
  return __ns.instantiate(await sourceBytes(source), importObject);
};
