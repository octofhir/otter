// The JS half of the native `WebAssembly` namespace. Runs as a
// `#[js_namespace]` factory glue: `__ns` is the `WebAssembly` namespace
// object and `natives` is the private compute bag (here it holds
// `buildInstance`, moved off the public object by the macro).
//
// This layer owns the pieces that are cleaner in JS: relocating the
// native reference-type constructors onto the namespace, the
// `CompileError` / `LinkError` / `RuntimeError` subclasses, the
// synchronous `Instance` class (which delegates to the native
// `buildInstance` and re-types its thrown error), the namespace brand,
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

// The three error interfaces are ordinary Error subclasses.
function makeErrorClass(name) {
  const ErrorClass = class extends Error {
    constructor(message) {
      super(message);
    }
  };
  Object.defineProperty(ErrorClass, 'name', { value: name, configurable: true });
  Object.defineProperty(ErrorClass.prototype, 'name', {
    value: name,
    writable: true,
    enumerable: false,
    configurable: true,
  });
  return ErrorClass;
}
__ns.CompileError = makeErrorClass('CompileError');
__ns.LinkError = makeErrorClass('LinkError');
__ns.RuntimeError = makeErrorClass('RuntimeError');

// `new WebAssembly.Instance(module, importObject)` is synchronous. The
// native `buildInstance` returns a fully formed instance object (own
// `exports`, prototype set to this class's prototype); a link/compile/
// runtime failure arrives as a `"<Kind>: <message>"` string that is
// re-typed into the matching error class here.
const Instance = class Instance {
  constructor(module, importObject) {
    try {
      return natives.buildInstance(module, importObject);
    } catch (error) {
      const text = String((error && error.message) || error);
      const separator = text.indexOf(': ');
      if (separator > 0) {
        const kind = text.slice(0, separator);
        const rest = text.slice(separator + 2);
        if (kind === 'LinkError') throw new __ns.LinkError(rest);
        if (kind === 'CompileError') throw new __ns.CompileError(rest);
        if (kind === 'RuntimeError') throw new __ns.RuntimeError(rest);
      }
      throw error;
    }
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

// A wasm export that `throw`s surfaces its exception to native code, which
// re-throws the JS value through this hidden re-thrower so the value's
// identity (a `WebAssembly.Exception`, or the original JS value carried by a
// `JSTag` exception) is preserved as the caught value.
Object.defineProperty(__ns, '__throw', {
  value: (value) => { throw value; },
  writable: false,
  enumerable: false,
  configurable: false,
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
