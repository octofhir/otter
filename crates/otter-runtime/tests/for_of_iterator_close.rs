//! Runtime coverage for §7.4.9 IteratorClose on abrupt completion of
//! a `for…of` loop and array destructuring.
//!
//! Each test drives a real `Runtime`, defines an iterable whose
//! iterator records `next`/`return` invocations in a global `log`
//! array, and asserts the iterator's `return` runs exactly when the
//! spec requires:
//!
//! - on `break` / `return` / a throw escaping the loop body → close;
//! - when iteration runs to completion or `next` itself throws → no
//!   close (the iterator record is already `[[done]]`);
//! - when a `try`/`catch` *inside* the body swallows the throw → no
//!   close (the loop never completes abruptly).
//!
//! Each snippet ends in `log.join(',')` so the script completion value
//! is the recorded call trace.

use otter_runtime::{Runtime, SourceInput};

fn run_trace(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<for-of-close-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

/// An iterable whose `next` always yields and whose `return` pushes
/// `return` to `log`. `next` logs each call.
const LIVE_ITERABLE: &str = "var iterable = {\n\
    [Symbol.iterator]() {\n\
      return {\n\
        next() { log.push('next'); return { value: 1, done: false }; },\n\
        return() { log.push('return'); return {}; }\n\
      };\n\
    }\n\
  };\n";

#[test]
fn close_on_break_runs_return() {
    let src = format!(
        "var log = [];\n{LIVE_ITERABLE}\
         for (const x of iterable) {{ log.push('body'); break; }}\n\
         log.join(',');"
    );
    assert_eq!(run_trace(&src), "next,body,return");
}

#[test]
fn close_on_return_runs_return() {
    let src = format!(
        "var log = [];\n\
         function f() {{\n{LIVE_ITERABLE}\
           for (const x of iterable) {{ log.push('body'); return; }}\n\
         }}\n\
         f();\n\
         log.join(',');"
    );
    assert_eq!(run_trace(&src), "next,body,return");
}

#[test]
fn close_on_throw_in_body_runs_return() {
    let src = format!(
        "var log = [];\n{LIVE_ITERABLE}\
         try {{\n\
           for (const x of iterable) {{ log.push('body'); throw new Error('boom'); }}\n\
         }} catch (e) {{ log.push('catch'); }}\n\
         log.join(',');"
    );
    assert_eq!(run_trace(&src), "next,body,return,catch");
}

#[test]
fn run_to_completion_does_not_close() {
    // A naturally exhausted iterator is `[[done]]`; IteratorClose is a
    // no-op (no `return` call).
    let src = "var log = [];\n\
         var iterable = {\n\
           [Symbol.iterator]() {\n\
             var i = 0;\n\
             return {\n\
               next() {\n\
                 log.push('next');\n\
                 return i++ < 1 ? { value: 1, done: false } : { value: undefined, done: true };\n\
               },\n\
               return() { log.push('return'); return {}; }\n\
             };\n\
           }\n\
         };\n\
         for (const x of iterable) { log.push('body'); }\n\
         log.join(',');";
    assert_eq!(run_trace(src), "next,body,next");
}

#[test]
fn inner_try_catch_does_not_close_outer_loop() {
    // A throw caught *inside* the loop body never completes the loop
    // abruptly, so the iterator stays open and iteration resumes.
    let src = "var log = [];\n\
         var iterable = {\n\
           [Symbol.iterator]() {\n\
             var i = 0;\n\
             return {\n\
               next() {\n\
                 log.push('next');\n\
                 return i++ < 2 ? { value: i, done: false } : { value: undefined, done: true };\n\
               },\n\
               return() { log.push('return'); return {}; }\n\
             };\n\
           }\n\
         };\n\
         for (const x of iterable) {\n\
           try { throw new Error('boom'); } catch (e) { log.push('caught'); }\n\
         }\n\
         log.join(',');";
    // Two yielded values, each caught inside the body, then natural
    // exhaustion — `return` is never invoked.
    assert_eq!(run_trace(src), "next,caught,next,caught,next");
}

#[test]
fn dstr_target_key_throwing_closes() {
    // The destructuring target key `{}[thrower()]` is evaluated before
    // `IteratorStep`, so `next` is never called; the iterator opened by
    // `GetIterator` is still `[[done]] === false` and is closed
    // (§7.4.9) — `return` runs without any `next`.
    let src = "var log = [];\n\
         var iterator = {\n\
           next() { log.push('next'); return { value: 1, done: false }; },\n\
           return() { log.push('return'); return {}; }\n\
         };\n\
         var iterable = {}; iterable[Symbol.iterator] = function () { return iterator; };\n\
         var thrower = function () { throw new Error('boom'); };\n\
         try { for ([ {}[thrower()] ] of [iterable]) {} } catch (e) {}\n\
         log.join(',');";
    assert_eq!(run_trace(src), "return");
}
