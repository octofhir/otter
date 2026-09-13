//! Ordinary closure receiver allocation with live prototype and weak profiles.
//!
//! # Contents
//! - Small/large sibling closures sharing one bytecode constructor template.
//! - Prototype replacement, inherited setters and lazy/null prototypes.
//!
//! # Invariants
//! - Native allocation is measured independently of compilation/JS output.
//! - Weak samples neither retain objects nor survive collection unchecked.
//! - Observable prototype/store behavior completes exactly once.

#![cfg(target_arch = "aarch64")]
use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn ordinary_receivers_use_live_prototypes_and_per_closure_capacity() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let warm = runtime.run_script(SourceInput::from_javascript(r#"
function factory(init) { return function(value) { init(this, value); }; }
var grow = false;
function smallInit(o, v) {
  o.x = v; o.y = v + 1;
  if (grow) { o.a = 1; o.b = 2; o.c = 3; o.d = 4; o.e = 5; }
}
function bigInit(o, v) { o.x = v; o.a = 1; o.b = 2; o.c = 3; o.d = 4; o.e = 5; o.f = 6; }
var small = factory(smallInit), big = factory(bigInit);
small.prototype = {kind: 'small'};
big.prototype = {kind: 'big'};
function build(Ctor, v) { return new Ctor(v); }
function drive(Ctor, n) { var sum = 0; for (var i = 0; i < n; i++) sum += build(Ctor, i).x; return sum; }
drive(big, 8000);
drive(small, 60000);
"#), "ordinary-receiver-warm.js").unwrap();
        assert_eq!(warm.completion_string(), "1799970000");
        let before = runtime.execution_stats();
        let probe = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
var settled = drive(small, 512);
var original = small.prototype;
small.prototype = {kind: 'replacement'};
var changed = build(small, 7);
var effects = 0;
var setterProto = {set x(v) { effects++; this.z = v; }};
small.prototype = setterProto;
var setterResult = build(small, 8);
small.prototype = null;
var nullResult = build(small, 9);
var lazy = factory(smallInit);
var lazyResult = build(lazy, 10);
JSON.stringify([settled, Object.getPrototypeOf(changed) !== original,
  changed.kind, changed.x, changed.y, effects, setterResult.z, setterResult.y,
  Object.getPrototypeOf(nullResult) === Object.prototype,
  Object.getPrototypeOf(lazyResult) === lazy.prototype, lazyResult.x]);
"#,
                ),
                "ordinary-receiver-probe.js",
            )
            .unwrap();
        assert_eq!(
            probe.completion_string(),
            "[130816,true,\"replacement\",7,8,1,8,9,true,true,10]"
        );
        let after = runtime.execution_stats();
        if std::env::var("OTTER_GC_STRESS")
            .ok()
            .is_none_or(|s| s == "0")
        {
            assert!(
                after.jit_receiver_alloc_generated - before.jit_receiver_alloc_generated >= 256,
                "{selection:?}: small siblings must allocate natively despite a large template maximum: {before:?} -> {after:?}"
            );
        }
        assert!(after.jit_generated_calls > before.jit_generated_calls);
        runtime.run_script(SourceInput::from_javascript(
            "small.prototype = original; var survivor = build(small, 15); survivor.child = {marker: 42}; undefined;"
        ), "ordinary-receiver-before-gc.js").unwrap();
        runtime.force_gc().unwrap();
        let after_gc = runtime.run_script(SourceInput::from_javascript(
            "JSON.stringify([survivor.x, survivor.child.marker, Object.getPrototypeOf(survivor) === small.prototype, drive(small, 512)]);"
        ), "ordinary-receiver-after-gc.js").unwrap();
        assert_eq!(after_gc.completion_string(), "[15,42,true,130816]");
        if std::env::var("OTTER_GC_STRESS")
            .ok()
            .is_none_or(|s| s == "0")
        {
            assert!(
                runtime.execution_stats().jit_receiver_alloc_generated
                    - after.jit_receiver_alloc_generated
                    >= 256,
                "{selection:?}: collection must permit a fresh weak observation and native allocation"
            );
        }
        let growth = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
small.prototype = original;
grow = true;
var firstWide = build(small, 11), secondWide = build(small, 12);
JSON.stringify([Object.keys(firstWide).length, Object.keys(secondWide).length,
  firstWide.x, secondWide.e, drive(big, 8)]);
"#,
                ),
                "ordinary-receiver-growth.js",
            )
            .unwrap();
        assert_eq!(growth.completion_string(), "[7,7,11,5,28]");
        let descriptors = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
grow = false;
var proxyReads = 0;
var wrapped = new Proxy(small, {get(target, key, receiver) {
  if (key === 'prototype') proxyReads++;
  return Reflect.get(target, key, receiver);
}});
var throughProxy = build(wrapped, 13);
Object.defineProperty(small, 'prototype', {value: {kind: 'fixed'}, writable: false});
var fixed = build(small, 14);
JSON.stringify([proxyReads, throughProxy.x, fixed.kind, fixed.x,
  Object.getOwnPropertyDescriptor(small, 'prototype').writable]);
"#,
                ),
                "ordinary-receiver-descriptors.js",
            )
            .unwrap();
        assert_eq!(descriptors.completion_string(), "[1,13,\"fixed\",14,false]");
    }
}
