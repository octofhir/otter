//! Runtime regression coverage for iterator-helper re-entry guards.
//!
//! # Contents
//! - Re-entering a currently running `map` helper throws `TypeError`.
//! - Catching the re-entry error inside the callback does not close the source.
//!
//! # Invariants
//! - Lazy iterator helpers have an executing flag independent from source
//!   iterator liveness.
//!
//! # See also
//! - <https://tc39.es/proposal-iterator-helpers/>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<iterator-helper-reentry>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn map_reentry_throws_type_error() {
    let completion = run(r#"
        const iter = [1].values();
        let helper;
        helper = iter.map(() => helper.next());
        let threw = false;
        try { helper.next(); } catch (error) { threw = error instanceof TypeError; }
        String(threw);
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn caught_map_reentry_does_not_close_source() {
    let completion = run(r#"
        const iter = [1, 2, 3].values();
        let helper;
        let reentered = false;
        helper = iter.map((value) => {
            if (value === 2) {
                try { helper.next(); } catch (error) {
                    reentered = error instanceof TypeError;
                }
            }
            return value;
        });
        const a = helper.next();
        const b = helper.next();
        const c = helper.next();
        const d = helper.next();
        [a.value, b.value, c.value, d.done, reentered].join(":");
        "#);
    assert_eq!(completion, "1:2:3:true:true");
}
