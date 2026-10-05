//! Scoped marshalling conversions and exact error-family proofs.
//!
//! # Contents
//! - Primitive, collection and binary conversions through a live native scope.
//! - Rooted prototype mutation, physical GC movement and cap refusal.
//! - Canonical native error carriers for underlying VM/binary failures.
//!
//! # Invariants
//! - Every test uses a real interpreter and production handle scope.
//! - Coercions that execute JavaScript are also covered by runtime suites.
//! - Existing `JsError::Native` preserves the exact native family and message;
//!   a carrier change must not weaken prototype or allocation assertions.
//!
//! # See also
//! - [`super::MarshalCx`] owns scoped conversions.
//! - [`crate::native_function::vm_to_native_error`] owns native error mapping.

use crate::binary::typed_array::TypedArrayKind;
use crate::promise::{JsPromise, PromiseState};
use crate::{Interpreter, NativeCallInfo, NativeCtx, NativeError, Value};

use super::{
    ArrayBuffer, BufferSource, DOMString, FromJs, HostRef, IntoJs, JsError, MarshalCx, Sequence,
    USVString, Uint8Array, ValueIdent,
};

fn with_cx<R>(f: impl FnOnce(&mut MarshalCx<'_, '_, '_>) -> R) -> R {
    let mut interp = Interpreter::new().expect("fixture interpreter bootstrap");
    NativeCtx::with_host_context(
        &mut interp,
        NativeCallInfo::call(Value::undefined()),
        None,
        |ctx| {
            ctx.scope(|scope| {
                let mut cx = MarshalCx::new(scope);
                f(&mut cx)
            })
        },
    )
}

#[test]
fn primitive_from_js_roundtrip() {
    with_cx(|cx| {
        let n = cx.number(41.5);
        assert_eq!(f64::from_js(cx, n, ValueIdent::Argument(0)).unwrap(), 41.5);

        let b = cx.boolean(true);
        assert!(bool::from_js(cx, b, ValueIdent::Argument(0)).unwrap());

        let s = cx.string("hello").unwrap();
        let text = USVString::from_js(cx, s, ValueIdent::Argument(0)).unwrap();
        assert_eq!(text.as_str(), "hello");

        let dom = DOMString::from_js(cx, s, ValueIdent::Argument(0)).unwrap();
        assert_eq!(dom.to_lossy_string(), "hello");
    });
}

#[test]
fn to_string_spec_covers_primitives() {
    with_cx(|cx| {
        let n = cx.number(42.0);
        let s = USVString::from_js(cx, n, ValueIdent::Argument(0)).unwrap();
        assert_eq!(s.as_str(), "42");

        let u = cx.undefined();
        let s = USVString::from_js(cx, u, ValueIdent::Argument(0)).unwrap();
        assert_eq!(s.as_str(), "undefined");
    });
}

#[test]
fn int_conversions_are_modular() {
    with_cx(|cx| {
        let v = cx.number(4_294_967_296.0 + 5.0);
        assert_eq!(u32::from_js(cx, v, ValueIdent::Argument(0)).unwrap(), 5);
        let v = cx.number(-1.0);
        assert_eq!(
            u32::from_js(cx, v, ValueIdent::Argument(0)).unwrap(),
            u32::MAX
        );
        assert_eq!(i32::from_js(cx, v, ValueIdent::Argument(0)).unwrap(), -1);
        let v = cx.number(f64::NAN);
        assert_eq!(i32::from_js(cx, v, ValueIdent::Argument(0)).unwrap(), 0);
    });
}

#[test]
fn option_reads_nullish_as_none() {
    with_cx(|cx| {
        let u = cx.undefined();
        assert_eq!(
            Option::<f64>::from_js(cx, u, ValueIdent::Argument(0)).unwrap(),
            None
        );
        let n = cx.null();
        assert_eq!(
            Option::<f64>::from_js(cx, n, ValueIdent::Argument(0)).unwrap(),
            None
        );
        let v = cx.number(7.0);
        assert_eq!(
            Option::<f64>::from_js(cx, v, ValueIdent::Argument(0)).unwrap(),
            Some(7.0)
        );
    });
}

#[test]
fn sequence_extracts_dense_arrays() {
    with_cx(|cx| {
        let arr = cx.array(3).unwrap();
        for (i, n) in [1.0, 2.0, 3.0].into_iter().enumerate() {
            let v = cx.number(n);
            cx.set_index(arr, i, v).unwrap();
        }
        let seq = Sequence::<f64>::from_js(cx, arr, ValueIdent::Argument(0)).unwrap();
        assert_eq!(seq.0, vec![1.0, 2.0, 3.0]);
    });
}

#[test]
fn sequence_element_error_names_the_element() {
    with_cx(|cx| {
        let arr = cx.array(1).unwrap();
        let obj = cx.object().unwrap();
        cx.set_index(arr, 0, obj).unwrap();
        // Object → number coercion needs an execution context; the test
        // context has none, so the element conversion must fail and name
        // the element.
        let err = Sequence::<f64>::from_js(cx, arr, ValueIdent::Argument(0)).unwrap_err();
        let JsError::Type(message) = err else {
            panic!("expected a TypeError, got {err:?}");
        };
        assert!(message.contains("element 0"), "message: {message}");
    });
}

#[test]
fn buffer_source_reads_views_and_buffers() {
    with_cx(|cx| {
        let bytes = vec![1u8, 2, 3, 4];
        let view = cx.uint8_array_from_bytes(bytes.clone()).unwrap();
        let src = BufferSource::from_js(cx, view, ValueIdent::Argument(0)).unwrap();
        assert_eq!(src.as_ref(), bytes.as_slice());

        let buffer = cx.array_buffer_from_bytes(bytes.clone()).unwrap();
        let src = BufferSource::from_js(cx, buffer, ValueIdent::Argument(0)).unwrap();
        assert_eq!(src.as_ref(), bytes.as_slice());

        let not_binary = cx.number(1.0);
        assert!(BufferSource::from_js(cx, not_binary, ValueIdent::Argument(2)).is_err());
    });
}

#[test]
fn into_js_builds_typed_array_and_buffer() {
    with_cx(|cx| {
        let bytes = vec![9u8, 8, 7];
        let out = Uint8Array(bytes.clone()).into_js(cx).unwrap();
        let raw = cx.escape(out);
        let heap = cx.ctx().heap();
        let view = raw.as_typed_array(heap).expect("expected a typed array");
        assert_eq!(view.kind(), TypedArrayKind::Uint8);
        assert_eq!(view.byte_length(heap), 3);

        let out = ArrayBuffer(bytes.clone()).into_js(cx).unwrap();
        let raw = cx.escape(out);
        let buffer = raw.as_array_buffer().expect("expected an ArrayBuffer");
        let copied = buffer.with_bytes(cx.ctx().heap(), <[u8]>::to_vec);
        assert_eq!(copied, bytes);
    });
}

#[test]
fn typed_array_from_bytes_rejects_misaligned_length() {
    with_cx(|cx| {
        let err = cx
            .typed_array_from_bytes(TypedArrayKind::Uint32, vec![0u8; 6])
            .unwrap_err();
        assert!(
            matches!(&err, JsError::Native(NativeError::TypeError { name: "marshal", reason })
                if reason == "byte length 6 is not a multiple of Uint32Array element width 4"),
            "got {err:?}"
        );
    });
}

#[test]
fn into_js_vec_builds_dense_array() {
    with_cx(|cx| {
        let out = vec![1.0f64, 2.0, 3.0].into_js(cx).unwrap();
        let seq = Sequence::<f64>::from_js(cx, out, ValueIdent::Argument(0)).unwrap();
        assert_eq!(seq.0, vec![1.0, 2.0, 3.0]);
    });
}

#[test]
fn promise_builders_settle() {
    with_cx(|cx| {
        let payload = cx.number(5.0);
        let fulfilled = cx.promise_fulfilled(payload).unwrap();
        let raw = cx.escape(fulfilled);
        let promise = raw.as_promise().expect("expected a promise");
        match promise.state(cx.ctx().heap()) {
            PromiseState::Fulfilled(value) => assert_eq!(value.as_f64(), Some(5.0)),
            other => panic!("expected fulfilled, got {other:?}"),
        }

        let reason = cx.string("nope").unwrap();
        let rejected = cx.promise_rejected(reason).unwrap();
        let raw = cx.escape(rejected);
        let promise = raw.as_promise().expect("expected a promise");
        assert!(matches!(
            promise.state(cx.ctx().heap()),
            PromiseState::Rejected(_)
        ));
    });
}

#[test]
fn host_ref_brand_checks() {
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Marker(u32);
    impl crate::object::HostObjectData for Marker {}
    #[derive(Debug, Clone)]
    struct Other;

    with_cx(|cx| {
        let object = cx.ctx().alloc_host_object(Marker(7)).unwrap();
        let handle = cx.park(Value::object(object));

        let host = HostRef::<Marker>::from_js(cx, handle, ValueIdent::This).unwrap();
        assert_eq!(host.snapshot(cx).unwrap(), Marker(7));
        assert_eq!(host.with(cx, |m| m.0).unwrap(), 7);

        assert!(HostRef::<Other>::from_js(cx, handle, ValueIdent::This).is_err());

        let plain = cx.object().unwrap();
        assert!(HostRef::<Marker>::from_js(cx, plain, ValueIdent::This).is_err());
    });
}

#[test]
fn host_instance_ancestry_resolves_parent_data() {
    use super::{HostAncestry, HostInstance, construct_instance};
    use std::any::{Any, TypeId};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Base(u32);
    impl HostAncestry for Base {}

    #[derive(Debug, Clone)]
    struct Derived {
        base: Base,
        extra: &'static str,
    }
    // What the declaration macro will emit for `extends = Base` +
    // `#[js(parent)] base`.
    impl HostAncestry for Derived {
        fn ancestor(&self, target: TypeId) -> Option<&dyn Any> {
            if target == TypeId::of::<Self>() {
                Some(self)
            } else {
                self.base.ancestor(target)
            }
        }
        fn ancestor_mut(&mut self, target: TypeId) -> Option<&mut dyn Any> {
            if target == TypeId::of::<Self>() {
                Some(self)
            } else {
                self.base.ancestor_mut(target)
            }
        }
    }

    with_cx(|cx| {
        let instance = construct_instance(
            cx,
            "Derived",
            Derived {
                base: Base(11),
                extra: "x",
            },
        )
        .unwrap();

        // Base-class access on a subclass instance resolves through the
        // ancestry walk — this is what lets Blob.prototype methods run
        // on a File.
        let base = HostRef::<Base>::from_js(cx, instance, ValueIdent::This).unwrap();
        assert_eq!(base.snapshot(cx).unwrap(), Base(11));

        let derived = HostRef::<Derived>::from_js(cx, instance, ValueIdent::This).unwrap();
        assert_eq!(derived.with(cx, |d| d.extra).unwrap(), "x");

        // Unrelated class → brand failure with the dedicated message.
        #[derive(Debug)]
        struct Unrelated;
        let err = HostRef::<Unrelated>::from_js(cx, instance, ValueIdent::This).unwrap_err();
        let JsError::Type(message) = err else {
            panic!("expected TypeError, got {err:?}");
        };
        assert!(message.contains("unrelated"), "message: {message}");
    });

    // Cell-level view/view_mut contract, independent of the VM.
    let mut cell = HostInstance::new(
        "Derived",
        Derived {
            base: Base(1),
            extra: "y",
        },
    );
    assert_eq!(cell.view::<Base>().unwrap(), &Base(1));
    cell.view_mut::<Base>().unwrap().0 = 2;
    assert_eq!(cell.view::<Base>().unwrap(), &Base(2));
    assert_eq!(cell.class_name(), "Derived");
}

#[test]
fn construct_instance_without_registered_class_yields_instance() {
    use super::{HostAncestry, construct_instance};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Lone(u8);
    impl HostAncestry for Lone {}

    with_cx(|cx| {
        // No global named "Lone" exists; the instance is still built and
        // branded, just with no class prototype to link.
        let instance = construct_instance(cx, "Lone", Lone(3)).unwrap();
        let host = HostRef::<Lone>::from_js(cx, instance, ValueIdent::This).unwrap();
        assert_eq!(host.snapshot(cx).unwrap(), Lone(3));
    });
}

#[test]
fn js_error_lowering_keeps_kinds_and_authored_message_text() {
    for (error, kind, message) in [
        (
            JsError::Type("exact type text".into()),
            crate::ErrorKind::TypeError,
            "exact type text",
        ),
        (
            JsError::Range("too big".into()),
            crate::ErrorKind::RangeError,
            "too big",
        ),
        (
            JsError::Dom {
                name: "NotSupportedError",
                message: "no".into(),
            },
            crate::ErrorKind::TypeError,
            "NotSupportedError: no",
        ),
    ] {
        assert_eq!(
            error.clone().into_native("Test.op"),
            crate::NativeError::SpecError {
                kind,
                message: message.into(),
            }
        );
        with_cx(|cx| {
            let value = cx.error_value(error).expect("actual class materialization");
            let current = cx.escape(value).as_object().expect("actual Error instance");
            let prototype = cx.ctx().interp_mut().error_classes.prototype(kind);
            assert!(crate::object::has_in_proto_chain(
                current,
                cx.heap(),
                prototype
            ));
            let property = cx.get(value, "message").expect("live message descriptor");
            assert_eq!(cx.as_string_lossy(property).as_deref(), Some(message));
        });
    }
    let imported = crate::NativeError::RangeError {
        name: "original native",
        reason: "original reason".into(),
    };
    assert_eq!(
        JsError::from_native(imported.clone()).into_native("outer boundary"),
        imported
    );
}

#[test]
fn handles_survive_interleaved_allocations() {
    // Mint handles, then force a burst of further allocations; every
    // earlier handle must still read back its value (the arena is
    // traced, so moving scavenges rewrite the slots).
    with_cx(|cx| {
        let first = cx.string("first").unwrap();
        let bytes = cx.uint8_array_from_bytes(vec![1, 2, 3]).unwrap();
        for i in 0..512 {
            let _ = cx.string(&format!("filler-{i}")).unwrap();
        }
        assert_eq!(cx.as_string_lossy(first).as_deref(), Some("first"));
        let src = BufferSource::from_js(cx, bytes, ValueIdent::Argument(0)).unwrap();
        assert_eq!(src.as_ref(), &[1, 2, 3]);
    });
}

#[test]
fn scoped_prototype_transition_moves_live_receiver_and_keeps_both_handles_current() {
    struct Funding([u64; 16384]);
    impl otter_gc::SafeTraceable for Funding {
        const TYPE_TAG: u8 = 0xe6;
        fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
            let _ = self.0[0];
        }
    }
    let mut interp = Interpreter::with_string_heap_cap(4 * 1024 * 1024)
        .expect("capped marshal fixture bootstrap");
    interp.gc_heap.set_gc_stress(0, false);
    NativeCtx::with_host_context(&mut interp, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|scope| {
            let mut cx = MarshalCx::new(scope);
            let prototype = cx.object().unwrap();
            let marker = cx.string("prototype-child").unwrap();
            cx.define(
                prototype,
                "marker",
                marker,
                crate::object::PropertyFlags::data_default(),
            )
            .unwrap();
            let receiver = cx.object().unwrap();
            let receiver_alias = cx.park(cx.escape(receiver));
            let receiver_before = cx.escape(receiver).as_object().unwrap().offset();
            let prototype_before = cx.escape(prototype).as_object().unwrap().offset();
            let marker_before = cx.escape(marker).as_string_gc().unwrap().offset();
            let before = cx.heap().gc_cycle_counts();
            cx.heap_mut()
                .alloc_old(Funding([0; 16384]))
                .expect("reclaimable transition funding");
            assert_eq!(
                cx.heap().gc_cycle_counts(),
                before,
                "setup cannot age fresh inputs"
            );
            let reserved = cx.heap().max_heap_bytes() - cx.heap().tracked_bytes();
            cx.heap_mut()
                .reserve_bytes_no_collect(reserved)
                .expect("fill effective cap without collection");
            assert_eq!(cx.heap().gc_cycle_counts(), before);
            cx.set_prototype(receiver, Some(prototype)).unwrap();
            cx.heap_mut().release_bytes(reserved);
            assert!(
                cx.heap().gc_cycle_counts().1 > before.1,
                "prototype preparation itself must full-collect"
            );
            assert_ne!(
                cx.escape(receiver).as_object().unwrap().offset(),
                receiver_before
            );
            assert_ne!(
                cx.escape(prototype).as_object().unwrap().offset(),
                prototype_before
            );
            assert_ne!(
                cx.escape(marker).as_string_gc().unwrap().offset(),
                marker_before
            );
            assert_eq!(cx.escape(receiver), cx.escape(receiver_alias));
            let actual =
                crate::object::prototype_value(cx.escape(receiver).as_object().unwrap(), cx.heap());
            assert_eq!(actual, Some(cx.escape(prototype)));
            let inherited = cx.get(receiver, "marker").unwrap();
            assert_eq!(cx.escape(inherited), cx.escape(marker));
            assert_eq!(
                cx.as_string_lossy(inherited).as_deref(),
                Some("prototype-child")
            );
            cx.set_prototype(receiver, None).unwrap();
            assert_eq!(cx.escape(receiver), cx.escape(receiver_alias));
            assert_eq!(
                crate::object::prototype_value(cx.escape(receiver).as_object().unwrap(), cx.heap()),
                None
            );
        })
    });
}

#[test]
fn scoped_prototype_transition_rejects_locked_receiver_without_changing_parent() {
    with_cx(|cx| {
        let receiver = cx.object().unwrap();
        let prototype = cx.object().unwrap();
        let mut object = cx.escape(receiver).as_object().unwrap();
        crate::object::prevent_extensions(&mut object, cx.heap_mut()).unwrap();
        let parent =
            crate::object::prototype_value(cx.escape(receiver).as_object().unwrap(), cx.heap());
        let error = cx.set_prototype(receiver, Some(prototype)).unwrap_err();
        assert!(
            matches!(&error, JsError::Native(NativeError::TypeError { name: "marshal", reason })
                if reason == &crate::VmError::TypeMismatch.to_string()),
            "got {error:?}"
        );
        assert_eq!(
            crate::object::prototype_value(cx.escape(receiver).as_object().unwrap(), cx.heap()),
            parent
        );
    });
}
