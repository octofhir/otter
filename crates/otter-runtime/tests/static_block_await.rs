//! `await` in a class static block: an error in the block itself, allowed in
//! an async function or async arrow inside it (§15.7.1).
//!
//! The check stopped at nested functions but not at async arrows, so a
//! bundle that starts work from a static block with `(async () => { await
//! … })()` did not compile at all (Stripe's marketing pages).

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> Result<String, String> {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .map(|value| value.completion_string().to_owned())
        .map_err(|error| format!("{error:?}"))
}

#[test]
fn an_async_arrow_in_a_static_block_may_await() {
    let source = "globalThis.seen = [];\
                  class C { static { (async () => { seen.push(await 1); })(); \
                                     (async function () { seen.push(await 2); })(); \
                                     const f = async x => await x; f(3).then(v => seen.push(v)); } }\
                  'compiled'";
    assert_eq!(run(source).as_deref(), Ok("compiled"));
}

#[test]
fn await_directly_in_a_static_block_is_a_syntax_error() {
    for source in [
        "class C { static { await 1; } }",
        "class C { static { if (true) { await 1; } } }",
        "class C { static { (() => { (async () => 1)(); })(); x = await 1; } }",
    ] {
        let error = run(source).expect_err(source);
        assert!(error.contains("await"), "{source}: {error}");
    }
}
