//! Regression coverage for guarded `Math.<method>(...)` intrinsic
//! calls. The fast path is valid only while the realm global still
//! points at the bootstrap native method and operands need no
//! observable object-to-primitive coercion.

use otter_runtime::Otter;

fn run(source: &str) {
    Otter::new()
        .blocking_run_script(source)
        .expect("script should run");
}

#[test]
fn math_call_uses_original_method_fast_path() {
    run(r#"
            if (Math.sqrt(9) !== 3) {
                throw new Error("bad sqrt");
            }
        "#);
}

#[test]
fn math_call_observes_method_overwrite() {
    run(r#"
            Math.sqrt = () => 7;
            if (Math.sqrt(9) !== 7) {
                throw new Error("overwritten Math.sqrt ignored");
            }
        "#);
}

#[test]
fn math_call_observes_global_replacement() {
    run(r#"
            globalThis.Math = { sqrt() { return 13; } };
            if (Math.sqrt(9) !== 13) {
                throw new Error("global Math replacement ignored");
            }
        "#);
}

#[test]
fn math_call_observes_lexical_shadow() {
    run(r#"
            let Math = { sqrt() { return 11; } };
            if (Math.sqrt(9) !== 11) {
                throw new Error("lexical Math shadow ignored");
            }
        "#);
}

#[test]
fn math_call_preserves_object_to_primitive() {
    run(r#"
            let hits = 0;
            const value = {
                valueOf() {
                    hits += 1;
                    return 9;
                }
            };
            if (Math.sqrt(value) !== 3 || hits !== 1) {
                throw new Error("object coercion skipped");
            }
        "#);
}
