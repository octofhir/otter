//! Runtime file-extension coverage for the TypeScript-first compiler path.
//!
//! These tests keep extension routing at the public runtime boundary instead
//! of adding more crate-private coverage to `src/lib.rs`.

use otter_runtime::Otter;

#[test]
fn run_file_executes_ts_module_type_syntax_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("dep.ts"),
        "export const value: number = 17;\n",
    )
    .expect("write dep");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            import { value } from "./dep.ts";
            interface Box { value: number }
            const boxed: Box = { value: value as number };
            function fail() { return undefined.x; }
            if (boxed.value !== 17) fail();
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .expect("run entry");
}

#[test]
fn run_file_executes_tsx_type_syntax_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.tsx"),
        r#"
            type Box<T> = { value: T };
            const boxed: Box<number> = { value: 23 as number };
            function fail() { return undefined.x; }
            if (boxed.value !== 23) fail();
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.tsx"))
        .expect("run entry");
}

#[test]
fn run_file_executes_mts_module_type_syntax_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("dep.mts"),
        "export const value: number = 31;\n",
    )
    .expect("write dep");
    std::fs::write(
        dir.path().join("entry.mts"),
        r#"
            import { value } from "./dep.mts";
            type Box = { value: number };
            const boxed: Box = { value };
            function fail() { return undefined.x; }
            if (boxed.value !== 31) fail();
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.mts"))
        .expect("run entry");
}

#[test]
fn run_file_executes_cts_script_type_syntax_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.cts"),
        r#"
            interface Box { value: number }
            const boxed: Box = { value: 43 as number };
            function fail() { return undefined.x; }
            if (boxed.value !== 43) fail();
        "#,
    )
    .expect("write entry");

    Otter::new()
        .blocking_run_file(dir.path().join("entry.cts"))
        .expect("run entry");
}
