//! Known primordial lookup, cache and realm semantics through the Node loader.
//!
//! # Contents
//! - One grouped program covering captured holders, members and callable ABI.
//! - Real ESM hosted imports in default and additional realms with distinct
//!   caches, of `internal/test/binding` under `--expose-internals`: the realm's
//!   primordials as Node's own tests reach them.
//! - The actual vendored typed-array inspection reconstruction.
//!
//! # Invariants
//! Only the current hosted bootstrap executes. No reference bootstrap, copied
//! runtime registry, raw VM value or synthetic source context enters this test.
//! Mutable intrinsic descriptors are restored before loading Node inspection.
//! Rust observations are owned completions after the actual script checkpoint.
//!
//! # See also
//! - `nodelib::bootstrap_realm` builds each realm's primordials eagerly.
//! - `Runtime::run_module_source_in_realm` owns real source/realm admission.

use otter_node::NodeApiBuilderExt;
use otter_runtime::{Runtime, SourceInput};

const PROGRAM: &str = r#"
(function primordialLookupProof(bag, mode, inspectEnabled) {
  'use strict';
  function check(accepted, label) {
    if (!accepted) throw new Error('primordial ' + mode + ': ' + label);
  }
  check(bag.globalThis === globalThis && bag.Array === Array && bag.Map === Map &&
        bag.Math === Math, 'current intrinsic holder identity');
  check(bag.SafeMap.prototype.__primordialProbeRealm === undefined,
        'new realm owns a distinct SafeMap/cache');
  bag.SafeMap.prototype.__primordialProbeRealm = mode;

  const define = Object.defineProperty;
  const descriptor = Object.getOwnPropertyDescriptor;
  const originalIsView = descriptor(ArrayBuffer, 'isView');
  const originalSlice = descriptor(Array.prototype, 'slice');
  const originalSize = descriptor(Map.prototype, 'size');
  const originalMax = descriptor(Math, 'max');
  const originalApply = descriptor(Reflect, 'apply');
  const originalCatch = descriptor(Promise.prototype, 'catch');
  const replaced = function replaced() { throw new Error('replaced intrinsic ran'); };
  try {
    const isView = bag.ArrayBufferIsView;
    const slice = bag.ArrayPrototypeSlice;
    const getSize = bag.MapPrototypeGetSize;
    const maxApply = bag.MathMaxApply;
    const reflectApply = bag.ReflectApply;
    const explicitCatch = bag.PromisePrototypeCatch;
    define(ArrayBuffer, 'isView', { value: replaced, configurable: true });
    define(Array.prototype, 'slice', { value: replaced, configurable: true });
    define(Map.prototype, 'size', { get: replaced, configurable: true });
    define(Math, 'max', { value: replaced, configurable: true });
    define(Reflect, 'apply', { value: replaced, configurable: true });
    define(Promise.prototype, 'catch', { value: replaced, configurable: true });

    check(isView === originalIsView.value && bag.ArrayBufferIsView === isView &&
          isView(new Uint8Array(1)) === true,
          'static members are the unbound intrinsics captured before user code');
    check(slice.length === 1 && slice([4, 5, 6], 1).join(',') === '5,6' &&
          bag.ArrayPrototypeSlice === slice,
          'uncurried prototype member uses explicit receiver');
    check(getSize(new Map([[1, 2]])) === 1 && bag.MapPrototypeGetSize === getSize,
          'accessor half keeps the captured getter');
    check(maxApply.length === 1 && maxApply([5, 8]) === 8 && bag.MathMaxApply === maxApply,
          'static Apply array ABI');
    const pushApply = bag.ArrayPrototypePushApply;
    const pushed = [1];
    check(pushApply.length === 2 && pushApply(pushed, [2, 3]) === 3 &&
          pushed.join(',') === '1,2,3',
          'prototype Apply receiver/list ABI');
    check(reflectApply === originalApply.value &&
          reflectApply(function reflected() { return this.marker; }, { marker: 19 }, []) === 19,
          'ReflectApply is the unbound intrinsic');
    check(explicitCatch.length === 1 &&
          explicitCatch(Promise.reject(6), (value) => value + 11) instanceof Promise,
          'uncurried PromisePrototypeCatch keeps the captured method');
    const resolve = bag.PromiseResolve;
    check(resolve.name === 'bound resolve' && resolve(3) instanceof Promise,
          'Promise statics are bound to Promise');
    check(bag.SymbolIterator === Symbol.iterator && bag.SymbolFor === Symbol.for,
          'well-known symbol/function priority');
  } finally {
    define(ArrayBuffer, 'isView', originalIsView);
    define(Array.prototype, 'slice', originalSlice);
    define(Map.prototype, 'size', originalSize);
    define(Math, 'max', originalMax);
    define(Reflect, 'apply', originalApply);
    define(Promise.prototype, 'catch', originalCatch);
  }

  check(Object.getPrototypeOf(bag) === null && bag.toString === undefined &&
        bag.__otterUnknownPrimordialProbe__ === undefined &&
        !('__otterUnknownPrimordialProbe__' in bag),
        'null-prototype bag holds only catalog names');

  const safePairs = [
    [bag.SafeMap, Map, 'SafeMap'], [bag.SafeSet, Set, 'SafeSet'],
    [bag.SafeWeakMap, WeakMap, 'SafeWeakMap'], [bag.SafeWeakSet, WeakSet, 'SafeWeakSet'],
    [bag.SafeWeakRef, WeakRef, 'SafeWeakRef'],
    [bag.SafeFinalizationRegistry, FinalizationRegistry, 'SafeFinalizationRegistry'],
  ];
  for (const [safe, unsafe, name] of safePairs) {
    check(typeof safe === 'function' && safe.name === name &&
          Object.getPrototypeOf(safe.prototype) === unsafe.prototype,
          'Safe constructor/prototype ' + name);
  }
  const safeMap = new bag.SafeMap([[1, 7]]);
  const safeSet = new bag.SafeSet([3, 5]);
  check(safeMap instanceof Map && safeMap.get(1) === 7 &&
        safeSet instanceof Set && safeSet.has(5), 'Safe native superclass construction');
  const arrayIterator = new bag.SafeArrayIterator([8, 9]);
  const stringIterator = new bag.SafeStringIterator('xy');
  check(arrayIterator[Symbol.iterator]() === arrayIterator &&
        arrayIterator.next().value === 8 && arrayIterator.next().value === 9 &&
        arrayIterator.next().done === true && stringIterator.next().value === 'x' &&
        stringIterator.next().value === 'y' && stringIterator.next().done === true,
        'Safe iterator identities/results');

  const typed = new Uint8Array([3, 5]);
  Object.setPrototypeOf(typed, null);
  check(bag.TypedArrayPrototypeGetSymbolToStringTag(typed) === 'Uint8Array',
        'symbol accessor with explicit typed-array receiver');
  if (inspectEnabled) {
    const inspect = process.getBuiltinModule('internal/util/inspect').inspect;
    const typedConstructor = bag.Uint8Array;
    let reconstructions = 0;
    bag.Uint8Array = function reconstruction(current) {
      check(current === typed, 'finite-domain reconstruction keeps the actual input');
      reconstructions++;
      return new typedConstructor(current);
    };
    try {
      check(Object.getPrototypeOf(typed) === null &&
            inspect(typed) === '[Uint8Array(2): null prototype] [ 3, 5 ]' &&
            reconstructions === 1,
            'actual finite-domain typed-array reconstruction');
    } finally {
      bag.Uint8Array = typedConstructor;
    }
  }
  check(bag.SafeMap.prototype.__primordialProbeRealm === mode && bag.globalThis === globalThis,
        'loader retained the current realm cache');
  return mode + ':primordials-ok';
})
"#;

#[test]
fn primordial_members_are_captured_at_bootstrap_and_owned_by_the_source_realm() {
    // No console/timer/rejection getter is entered before this first bootstrap
    // load. The Node installer only installs their lazy functions/descriptors.
    let mut node = Runtime::builder()
        .expose_internals(true)
        .with_node_apis()
        .build()
        .expect("Node runtime for the actual vendored inspection loader");
    let default = node
        .run_script(
            SourceInput::from_javascript(format!(
                "{PROGRAM}(process.getBuiltinModule('internal/test/binding').primordials, 'collision', true)"
            )),
            "primordials:node-default",
        )
        .expect("default Node live primordial/cache/descriptor/inspection proof");
    assert_eq!(default.completion_string(), "collision:primordials-ok");

    // Node's process/global installer is a default-realm singleton. Register
    // the actual public hosted catalog for the realm pair, without replaying
    // that installer or copying its process/require into the additional realm.
    let mut realms = Runtime::builder()
        .expose_internals(true)
        .with_nodejs_modules()
        .hosted_modules(otter_node::hosted_modules().iter().copied())
        .build()
        .expect("runtime with the actual Node hosted module catalog");
    realms
        .run_module_source(
            SourceInput::from_javascript(format!(
                "import {{ primordials }} from 'internal/test/binding';\n\
                 globalThis.__primordialProofResult = {PROGRAM}(primordials, 'collision', false);"
            )),
            "file:///primordials-default.mjs",
        )
        .expect("actual ESM-to-hosted loader in the default realm");
    let default = realms
        .run_script(
            SourceInput::from_javascript("globalThis.__primordialProofResult"),
            "primordials:default-result",
        )
        .expect("owned default realm completion");
    assert_eq!(default.completion_string(), "collision:primordials-ok");

    let realm = realms
        .create_realm()
        .expect("additional catalog-only realm");
    realms
        .run_module_source_in_realm(
            realm,
            SourceInput::from_javascript(format!(
                "import {{ primordials }} from 'internal/test/binding';\n\
                 if (typeof process !== 'undefined') throw new Error('copied process singleton');\n\
                 globalThis.__primordialProofResult = {PROGRAM}(primordials, 'undefined', false);"
            )),
            "file:///primordials-additional.mjs",
        )
        .expect("additional real ESM loader and distinct primordial cache");
    let additional = realms
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript("globalThis.__primordialProofResult"),
            "primordials:additional-result",
        )
        .expect("owned additional realm completion");
    assert_eq!(additional.completion_string(), "undefined:primordials-ok");

    realms
        .run_module_source(
            SourceInput::from_javascript(
                "import { primordials } from 'internal/test/binding';\n\
                 globalThis.__primordialRetainedRealm = primordials.SafeMap.prototype.__primordialProbeRealm;",
            ),
            "file:///primordials-default-retained.mjs",
        )
        .expect("default loader reuses its original hosted realm module");
    let retained = realms
        .run_script(
            SourceInput::from_javascript("globalThis.__primordialRetainedRealm"),
            "primordials:default-retained",
        )
        .expect("return to the original realm cache");
    assert_eq!(retained.completion_string(), "collision");
}
