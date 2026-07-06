//! Runtime regression coverage for `Iterator.from(...).return`.
//!
//! # Contents
//! - Wrapped iterator `return` ignores the caller argument.
//! - Missing wrapped `return` returns `{ done: true, value: undefined }`.
//! - Wrapped non-iterable iterators inherit `@@iterator`.
//! - `Iterator.from` reads `next` before probing `%Iterator%` inheritance.
//!
//! # Invariants
//! - Generator `.return(value)` is separate; `%WrapForValidIteratorPrototype%`
//!   closes with an empty completion.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-%wrapforvaliditeratorprototype%.return>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<iterator-from-return>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn wrapped_return_ignores_argument() {
    let completion = run(r#"
        const iter = {
            next: () => ({ done: false, value: 0 }),
            return: (value = "old return") => ({ done: true, value }),
        };
        const result = Iterator.from(iter).return("ignored");
        result.done + ":" + result.value;
        "#);
    assert_eq!(completion, "true:old return");
}

#[test]
fn wrapped_missing_return_fallback_value_is_undefined() {
    let completion = run(r#"
        const iter = {
            next: () => ({ done: false, value: 0 }),
            return: null,
        };
        const result = Iterator.from(iter).return("ignored");
        result.done + ":" + String(result.value);
        "#);
    assert_eq!(completion, "true:undefined");
}

#[test]
fn wrapped_return_reads_current_return_method_each_time() {
    let completion = run(r#"
        const iter = {
            next: () => ({ done: false, value: 0 }),
            return: () => ({ done: true, value: "first" }),
        };
        const wrap = Iterator.from(iter);
        const first = wrap.return().value;
        iter.return = () => ({ done: true, value: "second" });
        first + ":" + wrap.return().value;
        "#);
    assert_eq!(completion, "first:second");
}

#[test]
fn iterator_from_reads_next_before_instance_probe() {
    let completion = run(r#"
        const log = [];
        const iter = new Proxy({
            next: () => ({ done: false, value: 0 }),
        }, {
            get(target, key, receiver) {
                log.push("get:" + String(key));
                return Reflect.get(target, key, receiver);
            },
            getPrototypeOf(target) {
                log.push("proto");
                return Reflect.getPrototypeOf(target);
            }
        });
        Iterator.from(iter);
        log.join(",");
        "#);
    assert_eq!(completion, "get:Symbol(Symbol.iterator),get:next,proto");
}

#[test]
fn iterator_from_non_iterable_wrapper_has_iterator_method() {
    let completion = run(r#"
        class TestIterator {
            next() { return { done: false, value: 0 }; }
        }
        const iter = new TestIterator();
        const wrapper = Iterator.from(iter);
        String(Symbol.iterator in iter) + ":" +
            String(iter !== wrapper) + ":" +
            String(Symbol.iterator in wrapper);
        "#);
    assert_eq!(completion, "false:true:true");
}
