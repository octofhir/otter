use otter_vm_runtime::Otter;

#[test]
fn typed_array_from_applies_mapfn_with_index() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        const out = Uint8Array.from([2, 4, 6], (value, index) => value + index);
        if (out.length !== 3) throw new Error("unexpected length");
        if (out.join(",") !== "2,5,8") {
            throw new Error("mapFn result mismatch");
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}

#[test]
fn typed_array_from_uses_this_arg_for_mapfn() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        const ctx = { mul: 3 };
        const out = Uint8Array.from([1, 2, 3], function(value, index) {
            if (this !== ctx) throw new Error("thisArg was not used");
            return value * this.mul + index;
        }, ctx);
        if (out.join(",") !== "3,7,11") {
            throw new Error("thisArg mapping mismatch");
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}

#[test]
fn typed_array_from_rejects_non_callable_mapfn() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let threw = false;
        try {
            Uint8Array.from([1, 2], 123);
        } catch (e) {
            threw = e instanceof TypeError;
        }
        if (!threw) throw new Error("Expected TypeError for non-callable mapFn");
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}
