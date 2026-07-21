//! Focused host atom and scoped string tests.

use otter_runtime::{HostAtomInterner, Runtime, SourceInput};

#[test]
fn repeated_atom_lookup_reuses_stable_name_storage() {
    let atoms = HostAtomInterner::new();
    let marker = atoms.intern("marker");
    assert_eq!(marker.id(), atoms.intern("marker").id());

    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .eval_value(
            SourceInput::from_javascript("({ marker: 42, text: 'otter' })"),
            "<host-atom>",
            |ctx, object| {
                ctx.scope(|mut scope| {
                    let object = scope.value(object);
                    for _ in 0..50_000 {
                        assert!(scope.has_own_atom_property(object, &marker));
                    }

                    let text_atom = atoms.intern("text");
                    let text = scope.get_atom(object, &text_atom).expect("text");
                    let len = scope
                        .with_string_str(text, |borrowed| {
                            assert_eq!(borrowed, "otter");
                            borrowed.len()
                        })
                        .expect("borrow string");
                    assert_eq!(len, 5);
                    assert_eq!(scope.string_value(text).unwrap(), "otter");
                });
            },
        )
        .expect("host atom script");
}

#[test]
fn atom_boundary_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HostAtomInterner>();
    assert_send_sync::<otter_runtime::RuntimeHostAtom>();
    assert_send_sync::<otter_runtime::RuntimeHostAtomId>();
}
