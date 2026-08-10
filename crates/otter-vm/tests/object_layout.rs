//! Building an object from a known key list, rather than one `set` at a time.

use otter_vm::{Interpreter, NativeCallInfo, NativeCtx, Value};

/// Run `body` with a native context over a fresh interpreter.
fn with_context<R>(body: impl FnOnce(&mut NativeCtx<'_>) -> R) -> R {
    let mut interp = Interpreter::new();
    NativeCtx::with_host_context(&mut interp, NativeCallInfo::default_call(), None, body)
}

#[test]
fn a_layout_gives_an_object_the_keys_it_names() {
    let (present, absent) = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let layout = scope.object_layout(&["@id", "@name", "#text"]).unwrap();
            assert_eq!(layout.len(), 3);
            assert!(!layout.is_empty());
            let object = scope.object_of_layout(layout).unwrap();
            let present = ["@id", "@name", "#text"]
                .into_iter()
                .all(|key| scope.has_own_string_property(object, key));
            (present, scope.has_own_string_property(object, "@other"))
        })
    });
    assert!(present, "every key the layout names is an own property");
    assert!(!absent, "and nothing else is");
}

#[test]
fn slots_start_undefined_and_answer_to_their_names_once_written() {
    let (by_slot, by_name, empty) = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let layout = scope.object_layout(&["a", "b"]).unwrap();
            let object = scope.object_of_layout(layout).unwrap();
            let fresh = scope.slot(object, 0).unwrap();
            let empty = scope.is_undefined(fresh);

            let value = scope.string("first").unwrap();
            scope.set_slot(object, 0, value).unwrap();
            let value = scope.string("second").unwrap();
            scope.set_slot(object, 1, value).unwrap();

            let second_slot = scope.slot(object, 1).unwrap();
            let by_slot = scope.string_value(second_slot).unwrap();
            // The layout is a real hidden class, so an ordinary named read
            // finds the same value.
            let named = scope.get(object, "a").unwrap();
            let by_name = scope.string_value(named).unwrap();
            (by_slot, by_name, empty)
        })
    });
    assert!(empty, "an unwritten slot reads as undefined");
    assert_eq!(by_slot, "second");
    assert_eq!(by_name, "first");
}

#[test]
fn the_same_key_list_gives_the_same_layout() {
    let (first, second, other) = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let first = scope.object_layout(&["x", "y"]).unwrap();
            let second = scope.object_layout(&["x", "y"]).unwrap();
            let other = scope.object_layout(&["y", "x"]).unwrap();
            (first, second, other)
        })
    });
    assert_eq!(first, second, "one hidden class per key list");
    assert_ne!(first, other, "order is part of the layout");
}

#[test]
fn an_empty_layout_builds_an_object_with_nothing_on_it() {
    let has_any = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let layout = scope.object_layout(&[]).unwrap();
            assert!(layout.is_empty());
            let object = scope.object_of_layout(layout).unwrap();
            scope.has_own_string_property(object, "anything")
        })
    });
    assert!(!has_any);
}

#[test]
fn a_layout_object_survives_collection_between_its_slot_writes() {
    let values = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let layout = scope.object_layout(&["one", "two", "three"]).unwrap();
            let object = scope.object_of_layout(layout).unwrap();
            let mut written = Vec::new();
            for (index, text) in ["a", "b", "c"].into_iter().enumerate() {
                // Allocating between the writes gives the collector every
                // chance to move both the object and the strings.
                for _ in 0..64 {
                    let _ = scope.object().unwrap();
                }
                let value = scope.string(text).unwrap();
                scope.set_slot(object, index, value).unwrap();
            }
            for index in 0..3 {
                let slot = scope.slot(object, index).unwrap();
                written.push(scope.string_value(slot).unwrap());
            }
            written
        })
    });
    assert_eq!(values, vec!["a", "b", "c"]);
}

#[test]
fn a_layout_object_is_an_ordinary_object_to_everything_else() {
    let (is_object, is_array, extra) = with_context(|ctx| {
        ctx.scope(|mut scope| {
            let layout = scope.object_layout(&["k"]).unwrap();
            let object = scope.object_of_layout(layout).unwrap();
            let value = scope.number(1.0);
            scope.set_slot(object, 0, value).unwrap();
            // A key the layout never named still lands, through the ordinary
            // store, without disturbing the ones it did.
            let extra_value = scope.boolean(true);
            scope.set(object, "later", extra_value).unwrap();
            let later = scope.get(object, "later").unwrap();
            let extra = scope.boolean_value(later).unwrap();
            (scope.is_object(object), scope.is_exact_array(object), extra)
        })
    });
    assert!(is_object);
    assert!(!is_array);
    assert!(extra);
}

#[test]
fn a_slot_read_on_something_that_is_not_an_object_is_refused() {
    with_context(|ctx| {
        ctx.scope(|mut scope| {
            let text = scope.string("not an object").unwrap();
            assert!(scope.slot(text, 0).is_err());
            let value = scope.number(0.0);
            assert!(scope.set_slot(text, 0, value).is_err());
            let _ = Value::undefined();
        });
    });
}
