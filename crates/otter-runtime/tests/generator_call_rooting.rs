//! Generator-call rooting across prologue and prototype resolution.
//!
//! # Invariants
//!
//! - Direct and spread calls keep the newly-created generator rooted while its
//!   prologue allocates and its `fn.prototype` (a writable, non-configurable
//!   data property, §27.3.4) is resolved.
//! - Native synchronous re-entry (`Array.from` driving `@@iterator`) uses the
//!   same rooted generator ownership.
//! - Redefining a generator's `prototype` as an accessor is a TypeError and
//!   leaves the next generator call reusable.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn generator_calls_keep_owner_rooted_through_prototype_lookup() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                const allocateAndReturn = (value) => {
                    const churn = [{}, {}, {}];
                    churn.push({ nested: [1, 2, 3] });
                    return value;
                };

                const directProto = {};
                function* direct(value) { yield value; }
                direct.prototype = allocateAndReturn(directProto);
                const directGen = direct(11);

                const spreadProto = {};
                function* spread(value) { yield value; }
                spread.prototype = allocateAndReturn(spreadProto);
                const spreadGen = spread(...[13]);

                const asyncProto = {};
                async function* asyncDirect(value) { yield value; }
                asyncDirect.prototype = allocateAndReturn(asyncProto);
                const asyncGen = asyncDirect(15);

                const iterable = {
                    [Symbol.iterator]: function* () { yield 17; }
                };
                const from = Array.from(iterable);

                function* fails() { yield 19; }
                let caughtSame = false;
                try {
                    Object.defineProperty(fails, "prototype", {
                        configurable: true,
                        get() { return allocateAndReturn({}); }
                    });
                } catch (error) {
                    caughtSame = error instanceof TypeError;
                }
                fails();

                const after = direct(23);
                [
                    Object.getPrototypeOf(directGen) === directProto,
                    directGen.next().value,
                    Object.getPrototypeOf(spreadGen) === spreadProto,
                    spreadGen.next().value,
                    Object.getPrototypeOf(asyncGen) === asyncProto,
                    from[0],
                    caughtSame,
                    after.next().value
                ].join("|");
                "#,
            ),
            "<generator-call-rooting>",
        )
        .expect("generator rooting fixture");

    assert_eq!(
        result.completion_string(),
        "true|11|true|13|true|17|true|23"
    );
}
