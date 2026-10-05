//! Known primordial lookup, cache and realm semantics through the Node loader.
//!
//! # Contents
//! - One grouped program covering live holders, descriptors and callable ABI.
//! - Real ESM hosted imports in default and additional realms with distinct caches.
//! - The actual vendored typed-array inspection reconstruction.
//!
//! # Invariants
//! Only the current hosted bootstrap executes. No reference bootstrap, copied
//! runtime registry, raw VM value or synthetic source context enters this test.
//! Mutable intrinsic descriptors are restored before loading Node inspection.
//! Rust observations are owned completions after the actual script checkpoint.
//!
//! # See also
//! - `nodelib::bootstrap_realm` owns the current lazy Map/Proxy.
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
  const originalPrefix = descriptor(Array, 'bufferIsView');
  const originalSlice = descriptor(Array.prototype, 'slice');
  const originalSize = descriptor(Map.prototype, 'size');
  const originalMax = descriptor(Math, 'max');
  const originalApply = descriptor(Reflect, 'apply');
  const originalCatch = descriptor(Promise.prototype, 'catch');
  let prefixReads = 0, fallbackReads = 0, sliceReads = 0;
  let sizeCalls = 0, maxReads = 0;
  const collision = function collision(a, b) {
    check(this === Array, 'static bind keeps the selected prefix holder');
    return a + b;
  };
  const prefix = function prefix() {
    prefixReads++;
    return mode === 'undefined' ? undefined : collision;
  };
  const sliceProbe = function sliceProbe(a, b) {
    return this[0] + a + b;
  };
  const sizeProbe = function sizeProbe() {
    sizeCalls++;
    return this.seed;
  };
  const maximum = function maximum(a, b) {
    check(this === Math, 'static Apply supplies Math as this');
    return a + b;
  };
  try {
    define(Array, 'bufferIsView', { get: prefix, configurable: true });
    define(ArrayBuffer, 'isView', {
      get() { fallbackReads++; return originalIsView.value; },
      configurable: true,
    });
    const prefixDescriptor = descriptor(Array, 'bufferIsView');
    check(prefixDescriptor.get === prefix && prefixDescriptor.set === undefined &&
          prefixDescriptor.enumerable === false && prefixDescriptor.configurable === true,
          'live accessor descriptor');
    const selected = bag.ArrayBufferIsView;
    check(prefixReads === 1 && fallbackReads === 0,
          'Array prefix precedes ArrayBuffer and undefined stops');
    if (mode === 'undefined') {
      check(selected === undefined, 'present undefined is the result');
    } else {
      check(typeof selected === 'function' && selected(4, 7) === 11,
            'selected prefix callable');
      check(selected.length === 2 && selected.name === 'bound collision',
            'static bind name and arity');
    }
    define(Array, 'bufferIsView', { value: function changed() {}, configurable: true });
    check(bag.ArrayBufferIsView === selected && prefixReads === 1 && fallbackReads === 0,
          'cached result identity includes undefined');

    define(Array.prototype, 'slice', {
      get() { sliceReads++; return sliceProbe; }, configurable: true,
    });
    const slice = bag.ArrayPrototypeSlice;
    check(sliceReads === 1 && slice([4], 3, 9) === 16,
          'uncurried prototype member uses explicit receiver');
    check(slice.length === 1 && slice.name === 'bound call',
          'uncurry name and arity');
    define(Array.prototype, 'slice', { value: function laterSlice() {}, configurable: true });
    check(bag.ArrayPrototypeSlice === slice && sliceReads === 1,
          'prototype getter is read live once, then cache owns identity');

    define(Map.prototype, 'size', { get: sizeProbe, configurable: true });
    const getSize = bag.MapPrototypeGetSize;
    check(sizeCalls === 0 && getSize.length === 1 && getSize.name === 'bound call',
          'accessor half is captured without invoking it');
    const map = new Map();
    map.seed = 23;
    check(getSize(map) === 23 && sizeCalls === 1 && bag.MapPrototypeGetSize === getSize,
          'captured getter receiver and identity');

    define(Math, 'max', {
      get() { maxReads++; return maximum; }, configurable: true,
    });
    const maxApply = bag.MathMaxApply;
    check(maxReads === 1 && maxApply.length === 1 && maxApply.name === '' &&
          maxApply([5, 8]) === 13 && bag.MathMaxApply === maxApply,
          'static Apply array ABI and cached identity');

    const pushApply = bag.ArrayPrototypePushApply;
    const pushed = [1];
    check(pushApply.length === 2 && pushApply.name === '' &&
          pushApply(pushed, [2, 3]) === 3 && pushed.join(',') === '1,2,3' &&
          bag.ArrayPrototypePushApply === pushApply,
          'prototype Apply receiver/list ABI');

    const reflectApply = bag.ReflectApply;
    const reflectedReceiver = { marker: 19 };
    check(reflectApply !== originalApply.value && reflectApply.length === 3 &&
          reflectApply.name === 'bound apply' &&
          reflectApply(function reflected() { return this.marker; }, reflectedReceiver, []) === 19,
          'ReflectApply is the real bound static member');
    define(Reflect, 'apply', { value: function replacedApply() { throw new Error('wrong apply'); }, configurable: true });
    check(bag.ReflectApply === reflectApply,
          'static ReflectApply retains the cached callable identity');

    let catchReads = 0;
    define(Promise.prototype, 'catch', {
      get() { catchReads++; throw new Error('prototype catch derivation ran'); },
      configurable: true,
    });
    const explicitCatch = bag.PromisePrototypeCatch;
    const promiseReceiver = {
      catch(handler) {
        check(this === promiseReceiver, 'explicit catch supplies the actual receiver');
        return handler(6);
      },
    };
    check(catchReads === 0 && explicitCatch.length === 2 &&
          explicitCatch(promiseReceiver, (value) => value + 11) === 17 &&
          bag.PromisePrototypeCatch === explicitCatch,
          'true explicit PromisePrototypeCatch precedes prototype derivation');
    check(bag.SymbolIterator === Symbol.iterator && bag.SymbolFor === Symbol.for,
          'well-known symbol/function priority');
  } finally {
    if (originalPrefix) define(Array, 'bufferIsView', originalPrefix);
    else delete Array.bufferIsView;
    define(ArrayBuffer, 'isView', originalIsView);
    define(Array.prototype, 'slice', originalSlice);
    define(Map.prototype, 'size', originalSize);
    define(Math, 'max', originalMax);
    define(Reflect, 'apply', originalApply);
    define(Promise.prototype, 'catch', originalCatch);
  }

  // Existing Proxy writes replace its one Map entry. Restore the probed
  // entries before genuine vendored code imports them below.
  bag.ArrayBufferIsView = originalIsView.value.bind(ArrayBuffer);
  bag.ArrayPrototypeSlice = bag.uncurryThis(originalSlice.value);
  bag.MapPrototypeGetSize = bag.uncurryThis(originalSize.get);
  bag.MathMaxApply = (args) => originalApply.value(originalMax.value, Math, args);
  const overridden = { marker: mode };
  const floor = bag.MathFloor;
  bag.MathFloor = overridden;
  check(bag.MathFloor === overridden && 'MathFloor' in bag, 'known cache override');
  bag.MathFloor = floor;
  check(bag.__otterUnknownPrimordialProbe__ === undefined &&
        !('__otterUnknownPrimordialProbe__' in bag), 'closed unknown result');
  bag.__otterUnknownPrimordialProbe__ = overridden;
  check(bag.__otterUnknownPrimordialProbe__ === overridden &&
        '__otterUnknownPrimordialProbe__' in bag, 'unknown cache override uses existing Map');
  bag.__otterUnknownPrimordialProbe__ = undefined;
  check(!('__otterUnknownPrimordialProbe__' in bag) && bag[Symbol.iterator] === undefined,
        'undefined has and non-string read');
  check(bag.toString === Object.prototype.toString, 'inherited special-name priority');

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
fn primordial_members_are_live_until_cached_and_remain_owned_by_the_source_realm() {
    // No console/timer/rejection getter is entered before this first bootstrap
    // load. The Node installer only installs their lazy functions/descriptors.
    let mut node = Runtime::builder()
        .with_node_apis()
        .build()
        .expect("Node runtime for the actual vendored inspection loader");
    let default = node
        .run_script(
            SourceInput::from_javascript(format!(
                "{PROGRAM}(process.getBuiltinModule('internal/bootstrap/realm').primordials, 'collision', true)"
            )),
            "primordials:node-default",
        )
        .expect("default Node live primordial/cache/descriptor/inspection proof");
    assert_eq!(default.completion_string(), "collision:primordials-ok");

    // Node's process/global installer is a default-realm singleton. Register
    // the actual public hosted catalog for the realm pair, without replaying
    // that installer or copying its process/require into the additional realm.
    let mut realms = Runtime::builder()
        .with_nodejs_modules()
        .hosted_modules(otter_node::hosted_modules().iter().copied())
        .build()
        .expect("runtime with the actual Node hosted module catalog");
    realms
        .run_module_source(
            SourceInput::from_javascript(format!(
                "import {{ primordials }} from 'internal/bootstrap/realm';\n\
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
                "import {{ primordials }} from 'internal/bootstrap/realm';\n\
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
                "import { primordials } from 'internal/bootstrap/realm';\n\
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
