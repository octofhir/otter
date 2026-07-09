//! Unit tests for the marshalling layer.
//!
//! Each test opens a real interpreter + native context + handle scope
//! and drives conversions end to end against live GC values. Coercion
//! paths that re-enter user JS (`valueOf` ladders, custom iterators)
//! are exercised by the runtime-level integration suites; these tests
//! cover the context-free conversions, the binary builders, and the
//! error shapes.

use crate::binary::typed_array::TypedArrayKind;
use crate::promise::{JsPromise, PromiseState};
use crate::{Interpreter, NativeCtx, Value};

use super::{
    ArrayBuffer, BufferSource, DOMString, FromJs, HostRef, IntoJs, JsError, MarshalCx, Sequence,
    USVString, Uint8Array, ValueIdent,
};

fn with_cx<R>(f: impl FnOnce(&mut MarshalCx<'_, '_, '_>) -> R) -> R {
    let mut interp = Interpreter::new();
    let mut ctx = NativeCtx::new(&mut interp);
    ctx.scope(|ctx, s| {
        let mut cx = MarshalCx::new(ctx, s);
        f(&mut cx)
    })
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
        assert!(matches!(err, JsError::Type(_)), "got {err:?}");
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
fn js_error_lowering_keeps_kinds() {
    let native = JsError::Range("too big".to_string()).into_native("Test.op");
    assert!(matches!(
        native,
        crate::NativeError::RangeError {
            name: "Test.op",
            ..
        }
    ));
    let native = JsError::Dom {
        name: "NotSupportedError",
        message: "no".to_string(),
    }
    .into_native("Test.op");
    match native {
        crate::NativeError::TypeError { reason, .. } => {
            assert!(reason.contains("NotSupportedError"));
        }
        other => panic!("expected TypeError lowering, got {other:?}"),
    }
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
