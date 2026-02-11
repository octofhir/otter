use otter_vm_runtime::Otter;

#[test]
fn tagged_template_passes_cooked_and_raw_parts() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        const out = (function (strings, value) {
            if (!Array.isArray(strings)) throw new Error("strings must be an array");
            if (strings.length !== 2) throw new Error("wrong cooked length");
            if (!Array.isArray(strings.raw)) throw new Error("raw must be an array");
            if (strings.raw.length !== 2) throw new Error("wrong raw length");
            if (strings[0] !== "a\n") throw new Error("wrong cooked[0]");
            if (strings.raw[0] !== "a\\n") throw new Error("wrong raw[0]");
            if (strings[1] !== "b") throw new Error("wrong cooked[1]");
            if (strings.raw[1] !== "b") throw new Error("wrong raw[1]");
            if (value !== 42) throw new Error("wrong substitution value");
            return 1;
        })`a\n${42}b`;

        if (out !== 1) throw new Error("tag call failed");
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}

#[test]
fn tagged_template_preserves_member_call_receiver() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        const obj = {
            x: 10,
            tag(strings, value) {
                if (this !== obj) throw new Error("wrong receiver");
                if (strings[0] !== "v=") throw new Error("wrong cooked string");
                return this.x + value;
            }
        };

        const result = obj.tag`v=${7}`;
        if (result !== 17) throw new Error("wrong tagged result");
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}

#[test]
fn tagged_template_allows_invalid_escapes() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let called = false;
        (function (strings) {
            if (strings[0] !== undefined) {
                throw new Error("cooked value must be undefined");
            }
            if (strings.raw[0] !== "\\8") {
                throw new Error("raw value must preserve source text");
            }
            called = true;
        })`\8`;

        if (!called) throw new Error("tag function was not called");
        "#,
    );

    assert!(
        result.is_ok(),
        "eval_sync should succeed: {:?}",
        result.err()
    );
}
