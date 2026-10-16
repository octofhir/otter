//! A private method called at the head of an optional chain keeps its
//! receiver.
//!
//! `this.#m(x)?.[0]` and `this.#m(x)?.length` call `#m` with `this` bound
//! and then read from what it returned, exactly as `this.#m(x)[0]` does
//! (§13.3.9: the chain's base is the whole call). The page glue's canvas
//! context does this, and `this` came through as undefined.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn a_private_call_heading_an_optional_chain_keeps_this() {
    let result = run(r#"
        class A {
            v = 7;
            #m(x) { return [this.v, x]; }
            plain() { return this.#m(1)[0]; }
            element() { return this.#m(2)?.[0]; }
            member() { return this.#m(3)?.length; }
            called() { return this.#m?.(4)[1]; }
            negated() { return !!this.#m(5)?.[0]; }
        }
        const a = new A();
        const out = [];
        for (const name of ['plain', 'element', 'member', 'called', 'negated']) {
            try { out.push(String(a[name]())); } catch (error) { out.push(name + ': ' + error.message); }
        }
        out.join(',')
    "#);
    assert_eq!(result, "7,7,2,4,true");
}
