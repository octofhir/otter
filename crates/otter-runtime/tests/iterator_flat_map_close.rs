//! Runtime regression coverage for `Iterator.prototype.flatMap` close behavior.
//!
//! # Contents
//! - Inner iterator `next` abrupt completion closes the outer iterator.
//! - Inner iterator result accessors close the outer iterator on throw.
//! - Non-iterable mapper results close the outer iterator before throwing.
//!
//! # Invariants
//! - `flatMap` closes the source iterator for abrupt completions while
//!   acquiring or draining the inner iterator.
//!
//! # See also
//! - <https://tc39.es/proposal-iterator-helpers/#sec-iteratorprototype.flatmap>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<iterator-flat-map>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn flat_map_closes_source_when_inner_next_throws() {
    let completion = run(r#"
        class TestError extends Error {}
        class Source extends Iterator {
            closed = false;
            next() { return { done: false, value: 0 }; }
            return() { this.closed = true; return { done: true }; }
        }
        class Inner extends Iterator {
            next() { throw new TestError(); }
        }
        const source = new Source();
        const mapped = source.flatMap(() => new Inner());
        let threw = false;
        try { mapped.next(); } catch (error) { threw = error instanceof TestError; }
        String(threw) + ":" + String(source.closed);
        "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn flat_map_closes_source_when_inner_result_accessor_throws() {
    let completion = run(r#"
        class TestError extends Error {}
        class Source extends Iterator {
            closed = false;
            next() { return { done: false, value: 0 }; }
            return() { this.closed = true; return { done: true }; }
        }
        class Inner extends Iterator {
            next() {
                return { get done() { throw new TestError(); } };
            }
        }
        const source = new Source();
        const mapped = source.flatMap(() => new Inner());
        let threw = false;
        try { mapped.next(); } catch (error) { threw = error instanceof TestError; }
        String(threw) + ":" + String(source.closed);
        "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn flat_map_closes_source_when_mapper_result_is_not_iterable() {
    let completion = run(r#"
        class Source extends Iterator {
            closed = false;
            next() { return { done: false, value: 0 }; }
            return() { this.closed = true; return { done: true }; }
        }
        const source = new Source();
        const mapped = source.flatMap(() => ({}));
        let threw = false;
        try { mapped.next(); } catch (error) { threw = error instanceof TypeError; }
        String(threw) + ":" + String(source.closed);
        "#);
    assert_eq!(completion, "true:true");
}
