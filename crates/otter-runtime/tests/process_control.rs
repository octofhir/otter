//! `process` control members: `chdir`, `kill`, `_kill`.
//!
//! # Contents
//! - `chdir` moves the process and `cwd()` reports the move.
//! - Argument and signal validation carry Node's codes and messages.
//! - `kill` dispatches through the object's own `_kill`.
//! - A denied read capability stops the move.
//!
//! # Invariants
//! - Every assertion here is the shape Node's own `test/parallel` files check,
//!   so a passing test means the corpus test can pass too.

use otter_runtime::{CapabilitySet, Permission, Runtime, SourceInput};

fn runtime_with(capabilities: CapabilitySet) -> Runtime {
    Runtime::builder()
        .capabilities(capabilities)
        .with_nodejs_modules()
        .build()
        .expect("runtime")
}

fn run(source: &str) -> Result<(), String> {
    runtime_with(CapabilitySet::allow_all())
        .run_script(SourceInput::from_javascript(source), "<test>")
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

#[test]
fn chdir_moves_the_process_and_cwd_follows() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target = std::fs::canonicalize(directory.path()).expect("canonical target");
    let target = serde_json::to_string(&target.to_string_lossy()).expect("encode path");

    run(&format!(
        r#"
        const before = process.cwd();
        process.chdir({target});
        if (process.cwd() !== {target}) {{
            throw new Error("cwd did not follow chdir: " + process.cwd());
        }}
        process.chdir(before);
        if (process.cwd() !== before) {{
            throw new Error("cwd did not return: " + process.cwd());
        }}
        "#
    ))
    .expect("chdir round-trip");
}

#[test]
fn chdir_reports_a_missing_directory_as_enoent() {
    run(r#"
        let thrown;
        try {
            process.chdir("this-directory-does-not-exist");
        } catch (error) {
            thrown = error;
        }
        if (!thrown) throw new Error("chdir to a missing directory must throw");
        if (thrown.code !== "ENOENT") throw new Error("code was " + thrown.code);
        if (!/ENOENT: no such file or directory, chdir /.test(thrown.message)) {
            throw new Error("message was " + thrown.message);
        }
        "#)
    .expect("chdir reports ENOENT");
}

#[test]
fn a_failed_chdir_carries_the_system_error_properties() {
    run(r#"
        let thrown;
        try {
            process.chdir("this-directory-does-not-exist");
        } catch (error) {
            thrown = error;
        }
        if (thrown.syscall !== "chdir") throw new Error("syscall was " + thrown.syscall);
        if (thrown.dest !== "this-directory-does-not-exist") {
            throw new Error("dest was " + thrown.dest);
        }
        if (thrown.path !== process.cwd()) throw new Error("path was " + thrown.path);
        if (thrown.errno !== -2) throw new Error("errno was " + thrown.errno);
        "#)
    .expect("a failed chdir reports errno, syscall, path, and dest");
}

#[test]
fn chdir_rejects_a_non_string_directory() {
    run(r#"
        let thrown;
        try {
            process.chdir(42);
        } catch (error) {
            thrown = error;
        }
        if (thrown?.code !== "ERR_INVALID_ARG_TYPE") throw new Error("code was " + thrown?.code);
        if (thrown.name !== "TypeError") throw new Error("name was " + thrown.name);
        "#)
    .expect("chdir validates its argument");
}

#[test]
fn chdir_requires_the_read_capability() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target = std::fs::canonicalize(directory.path()).expect("canonical target");
    let target = serde_json::to_string(&target.to_string_lossy()).expect("encode path");

    let mut capabilities = CapabilitySet::allow_all();
    capabilities.read = Permission::Deny;
    let source = format!(
        r#"
        let thrown;
        try {{
            process.chdir({target});
        }} catch (error) {{
            thrown = error;
        }}
        if (thrown?.code !== "EACCES") throw new Error("code was " + thrown?.code);
        "#
    );

    runtime_with(capabilities)
        .run_script(SourceInput::from_javascript(source), "<test>")
        .expect("a denied chdir throws instead of moving the process");
}

#[test]
fn kill_validates_the_pid_the_way_node_does() {
    run(r#"
        const rejected = ["SIGTERM", null, undefined, NaN, Infinity, -Infinity];
        for (const value of rejected) {
            let thrown;
            try {
                process.kill(value);
            } catch (error) {
                thrown = error;
            }
            if (thrown?.code !== "ERR_INVALID_ARG_TYPE") {
                throw new Error("pid " + String(value) + " gave " + thrown?.code);
            }
            if (thrown.name !== "TypeError") throw new Error("name was " + thrown.name);
        }
        if (!/Received type string \('SIGTERM'\)/.test((() => {
            try { process.kill("SIGTERM"); } catch (error) { return error.message; }
        })())) {
            throw new Error("the received tail is missing");
        }
        "#)
    .expect("kill rejects a pid that is not a whole number");
}

#[test]
fn kill_reports_an_unknown_signal_and_an_invalid_signal_number() {
    run(r#"
        let named;
        try { process.kill(0, "test"); } catch (error) { named = error; }
        if (named?.code !== "ERR_UNKNOWN_SIGNAL") throw new Error("code was " + named?.code);
        if (named.message !== "Unknown signal: test") throw new Error(named.message);

        let numbered;
        try { process.kill(0, 987); } catch (error) { numbered = error; }
        if (numbered?.code !== "EINVAL") throw new Error("code was " + numbered?.code);
        if (numbered.name !== "Error") throw new Error("name was " + numbered.name);
        if (numbered.message !== "kill EINVAL") throw new Error(numbered.message);
        "#)
    .expect("kill maps signal failures to Node's codes");
}

#[test]
fn kill_dispatches_through_the_objects_own_raw_kill() {
    run(r#"
        const seen = [];
        const original = process._kill;
        process._kill = function (pid, signal) {
            seen.push([pid, signal]);
            return 0;
        };
        process.kill(0, "SIGHUP");
        process.kill("0", undefined);
        process.kill(0, 15);
        process._kill = original;

        const expected = [[0, 1], [0, 15], [0, 15]];
        if (JSON.stringify(seen) !== JSON.stringify(expected)) {
            throw new Error("dispatched " + JSON.stringify(seen));
        }
        "#)
    .expect("kill routes through the replaceable _kill");
}
