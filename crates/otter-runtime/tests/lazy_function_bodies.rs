//! Functions a script does not run as soon as it creates them compile on
//! their first call, once, and behave exactly as if compiled with the
//! script.
//!
//! # Invariants
//!
//! - Loading a script compiles no body it does not run.
//! - A body compiles on its first call and never again.
//! - Name, length, `toString`, stack positions, construction, `this`,
//!   `super`, private names, closures over the creating frame, generators
//!   and async functions observe nothing of the deferral.

use otter_runtime::{Runtime, SourceInput};

fn run(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), "file:///lazy.js")
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn a_body_compiles_on_its_first_call_and_only_once() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let before = runtime.lazy_function_bodies_compiled();
    run(
        &mut runtime,
        "function never() { return 1; } function twice(x) { return x + 1; } var arrow = (a) => a * 2;",
    );
    assert_eq!(runtime.lazy_function_bodies_compiled(), before);
    assert_eq!(run(&mut runtime, "twice(1) + twice(2) + arrow(3)"), "11");
    assert_eq!(runtime.lazy_function_bodies_compiled(), before + 2);
    assert_eq!(run(&mut runtime, "twice(10) + arrow(10)"), "31");
    assert_eq!(runtime.lazy_function_bodies_compiled(), before + 2);
    // An immediately called function compiles with its script.
    assert_eq!(run(&mut runtime, "(function () { return 7; })()"), "7");
    assert_eq!(runtime.lazy_function_bodies_compiled(), before + 2);
    // Eval code defers its functions the same way, closing over the eval's
    // own lexicals, a direct eval's caller and strictness alike.
    let evaled = run(
        &mut runtime,
        r#"
const indirect = (0, eval)("let hidden = 4; function get() { return hidden; } get");
function host(x) {
  return eval("'use strict'; function g(y) { return x + y + (this === undefined); } g")(1);
}
const made = new Function("a", "function inner(b) { return a * b; } return inner;")(6);
[indirect(), host(2), made(7), typeof hidden].join()
"#,
    );
    assert_eq!(evaled, "4,4,42,undefined");
    // `host`, `get`, `g`, the `Function` body and `inner`.
    assert_eq!(runtime.lazy_function_bodies_compiled(), before + 7);
}

#[test]
fn a_lazily_compiled_function_observes_nothing_of_the_deferral() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let source = r#"
let counter = 0;
function outer(x) {
  let y = x * 2;
  const inner = (z) => y + z + counter;
  function deep(k) { return inner(k) * 2; }
  return deep(1);
}
const obj = {
  m(a) { return a + 1; },
  get g() { return 42; },
  set s(v) { counter = v; },
  async am() { return 5; },
  *gen() { yield 1; yield 2; },
  sup() { return super.hasOwnProperty === Object.prototype.hasOwnProperty; },
};
class C {
  #p = 3;
  constructor(v) { this.v = v; }
  get pv() { return this.#p + this.v; }
  meth(a) { return this.v + a; }
  #priv() { return this.#p * 10; }
  callPriv() { return this.#priv(); }
}
class D extends C {
  constructor(v) { const f = () => super(v + 1); f(); }
  meth(a) { return super.meth(a) * 2; }
}
function Ctor(x) { this.x = x; }
function add(a, b) { return a + b; }
function sloppyThis() { return typeof this; }
function args() { return arguments.length + arguments[1]; }
function thrower() { throw new Error('boom'); }
let line;
try { thrower(); } catch (e) { line = e.stack.split('\n')[1].trim(); }
const out = [
  outer(5), obj.m(1), obj.g, (obj.s = 7, counter), [...obj.gen()].join(','), obj.sup(),
  new C(1).pv, new C(1).meth(2), new C(1).callPriv(), new D(3).meth(1), new D(3).v,
  new Ctor(9).x, add.toString(), obj.m.name, add.length, sloppyThis.call(5), args(1, 2, 3),
  line,
];
let caught;
try { C.prototype.callPriv.call({}); } catch (e) { caught = e.constructor.name; }
out.push(caught);
out.join(' | ')
"#;
    assert_eq!(
        run(&mut runtime, source),
        "22 | 2 | 42 | 7 | 1,2 | true | 4 | 3 | 30 | 10 | 4 | 9 | \
         function add(a, b) { return a + b; } | m | 2 | object | 5 | \
         at thrower (file:///lazy.js:33:28) | TypeError"
    );
    assert!(runtime.lazy_function_bodies_compiled() > 10);
}

#[test]
fn an_error_in_a_lazily_compiled_body_is_an_early_error() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    for source in [
        "function f() { 'use strict'; with ({}) {} }",
        "function f(a) { let a; }",
        "function f() { this.#x; }",
        "({ m() { super(); } })",
        "function f() { 'use strict'; function g(a, a) {} }",
    ] {
        assert!(
            runtime
                .run_script(SourceInput::from_javascript(source), "file:///early.js")
                .is_err(),
            "`{source}` loads"
        );
    }
}

#[test]
fn a_body_called_from_another_chunk_runs_compiled_after_its_first_call() {
    // `inc` lives in the first script's chunk; native callbacks from the
    // second resolve its owner across chunks, before and after its body
    // compiles.
    let mut runtime = Runtime::builder().build().expect("runtime");
    run(&mut runtime, "function inc(x) { return x + 1; }");
    assert_eq!(run(&mut runtime, "[1, 2, 3].map(inc).join()"), "2,3,4");
    assert_eq!(run(&mut runtime, "[4, 5].map(inc).join()"), "5,6");
    assert_eq!(
        run(
            &mut runtime,
            "function twice(f, x) { return f(f(x)); } [twice(inc, 1), twice(inc, 2)].join()"
        ),
        "3,4"
    );
}
