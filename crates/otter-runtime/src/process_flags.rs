//! Immutable Node environment-option allowlist.
//!
//! `process.allowedNodeEnvironmentFlags` is a real Set snapshot with Node's
//! spelling aliases for `has()`. Host sealing prevents mutation through both
//! instance methods and borrowed `%Set.prototype%` mutators.
//!
//! # Contents
//! - [`build`] constructs and seals the JS-visible Set.
//! - [`allowed_flags_has`] implements Node's dash/underscore lookup aliases.
//!
//! # Invariants
//! - Iteration exposes canonical dashed flag names only.
//! - Mutation never changes the snapshot, including prototype-borrowed calls.
//! - Construction uses [`NativeCtx`] handle scopes and rooted collection APIs.
//!
//! # See also
//! - <https://nodejs.org/api/process.html#processallowednodeenvironmentflags>

use otter_vm::{Attr, HandleScope, NativeCall, NativeCtx, NativeError, Scoped, Value};

const ALLOWED_NODE_ENVIRONMENT_FLAGS: &[&str] = &[
    "-C",
    "-r",
    "--abort-on-uncaught-exception",
    "--addons",
    "--allow-addons",
    "--allow-child-process",
    "--allow-fs-read",
    "--allow-fs-write",
    "--allow-inspector",
    "--allow-wasi",
    "--allow-worker",
    "--async-context-frame",
    "--conditions",
    "--debug-arraybuffer-allocations",
    "--deprecation",
    "--diagnostic-dir",
    "--disable-proto",
    "--disable-sigusr1",
    "--disable-warning",
    "--disable-wasm-trap-handler",
    "--disallow-code-generation-from-strings",
    "--dns-result-order",
    "--enable-etw-stack-walking",
    "--enable-fips",
    "--enable-network-family-autoselection",
    "--enable-source-maps",
    "--entry-url",
    "--es-module-specifier-resolution",
    "--experimental-abortcontroller",
    "--experimental-addon-modules",
    "--experimental-detect-module",
    "--experimental-eventsource",
    "--experimental-fetch",
    "--experimental-global-customevent",
    "--experimental-global-navigator",
    "--experimental-global-webcrypto",
    "--experimental-import-meta-resolve",
    "--experimental-json-modules",
    "--experimental-loader",
    "--experimental-modules",
    "--experimental-print-required-tla",
    "--experimental-quic",
    "--experimental-repl-await",
    "--experimental-report",
    "--experimental-require-module",
    "--experimental-shadow-realm",
    "--experimental-specifier-resolution",
    "--experimental-sqlite",
    "--experimental-strip-types",
    "--experimental-test-isolation",
    "--experimental-top-level-await",
    "--experimental-transform-types",
    "--experimental-vm-modules",
    "--experimental-wasi-unstable-preview1",
    "--experimental-websocket",
    "--experimental-webstorage",
    "--experimental-wasm-modules",
    "--experimental-worker",
    "--expose-gc",
    "--extra-info-on-fatal-exception",
    "--force-async-hooks-checks",
    "--force-context-aware",
    "--force-fips",
    "--force-node-api-uncaught-exceptions-policy",
    "--frozen-intrinsics",
    "--global-search-paths",
    "--heapsnapshot-near-heap-limit",
    "--heapsnapshot-signal",
    "--http-parser",
    "--import",
    "--input-type",
    "--insecure-http-parser",
    "--interpreted-frames-native-stack",
    "--jitless",
    "--loader",
    "--localstorage-file",
    "--max-heap-size",
    "--max-http-header-size",
    "--max-old-space-size",
    "--max-old-space-size-percentage",
    "--max-semi-space-size",
    "--napi-modules",
    "--network-family-autoselection",
    "--network-family-autoselection-attempt-timeout",
    "--no-debug-arraybuffer-allocations",
    "--no-node-snapshot",
    "--no-trace-promises",
    "--no-verify-base-objects",
    "--node-memory-debug",
    "--node-snapshot",
    "--openssl-config",
    "--openssl-legacy-provider",
    "--openssl-shared-config",
    "--pending-deprecation",
    "--perf-basic-prof",
    "--perf-basic-prof-only-functions",
    "--perf-prof",
    "--perf-prof-unwinding-info",
    "--permission",
    "--preserve-symlinks",
    "--preserve-symlinks-main",
    "--prof-process",
    "--redirect-warnings",
    "--report-compact",
    "--report-dir",
    "--report-directory",
    "--report-exclude-env",
    "--report-exclude-network",
    "--report-filename",
    "--report-on-fatalerror",
    "--report-on-signal",
    "--report-signal",
    "--report-uncaught-exception",
    "--require",
    "--require-module",
    "--secure-heap",
    "--secure-heap-min",
    "--snapshot-blob",
    "--stack-trace-limit",
    "--strip-types",
    "--test-coverage-branches",
    "--test-coverage-exclude",
    "--test-coverage-functions",
    "--test-coverage-include",
    "--test-coverage-lines",
    "--test-global-setup",
    "--test-isolation",
    "--test-name-pattern",
    "--test-only",
    "--test-random-seed",
    "--test-randomize",
    "--test-reporter",
    "--test-reporter-destination",
    "--test-rerun-failures",
    "--test-shard",
    "--test-skip-pattern",
    "--throw-deprecation",
    "--title",
    "--tls-cipher-list",
    "--tls-keylog",
    "--tls-max-v1.2",
    "--tls-max-v1.3",
    "--tls-min-v1.0",
    "--tls-min-v1.1",
    "--tls-min-v1.2",
    "--tls-min-v1.3",
    "--trace-deprecation",
    "--trace-env",
    "--trace-env-js-stack",
    "--trace-env-native-stack",
    "--trace-event-categories",
    "--trace-event-file-pattern",
    "--trace-events-enabled",
    "--trace-exit",
    "--trace-promises",
    "--trace-require-module",
    "--trace-sigint",
    "--trace-sync-io",
    "--trace-tls",
    "--trace-uncaught",
    "--trace-warnings",
    "--track-heap-objects",
    "--unhandled-rejections",
    "--use-bundled-ca",
    "--use-env-proxy",
    "--use-largepages",
    "--use-openssl-ca",
    "--use-system-ca",
    "--v8-pool-size",
    "--verify-base-objects",
    "--watch",
    "--watch-kill-signal",
    "--watch-path",
    "--watch-preserve-output",
    "--warnings",
    "--zero-fill-buffers",
];

pub(crate) fn build<'s>(
    ctx: &mut NativeCtx<'_>,
    scope: &'s HandleScope,
) -> Result<Scoped<'s>, NativeError> {
    let flags = ctx.scoped_collection_set(scope)?;
    for flag in ALLOWED_NODE_ENVIRONMENT_FLAGS {
        let value = ctx.scoped_string(scope, flag)?;
        let mut set = ctx
            .escape(flags)
            .as_set()
            .expect("scoped_collection_set must produce a Set");
        ctx.set_add(&mut set, ctx.escape(value))
            .map_err(|error| NativeError::TypeError {
                name: "process.allowedNodeEnvironmentFlags",
                reason: error.to_string(),
            })?;
    }
    let has = ctx.scoped_native_call(scope, "has", 1, NativeCall::Static(allowed_flags_has))?;
    ctx.scoped_define_data(
        scope,
        flags,
        "has",
        has,
        Attr {
            writable: false,
            enumerable: false,
            configurable: false,
        }
        .to_flags(),
    )?;
    ctx.make_set_readonly(ctx.escape(flags))?;
    Ok(flags)
}

fn allowed_flags_has(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(flag) = args.first().and_then(|value| value.as_string(ctx.heap())) else {
        return Ok(Value::boolean(false));
    };
    let flag = flag.to_lossy_string(ctx.heap());
    let Some(canonical) = canonical_flag_name(&flag) else {
        return Ok(Value::boolean(false));
    };
    Ok(Value::boolean(
        ALLOWED_NODE_ENVIRONMENT_FLAGS.contains(&canonical.as_str()),
    ))
}

fn canonical_flag_name(flag: &str) -> Option<String> {
    let name = flag.split_once('=').map_or(flag, |(name, _)| name);
    if name.is_empty() || name.starts_with("---") {
        return None;
    }
    let name = name.replace('_', "-");
    if name == "r" {
        Some("-r".to_string())
    } else if name.starts_with('-') {
        Some(name)
    } else {
        Some(format!("--{name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::canonical_flag_name;

    #[test]
    fn canonicalizes_node_flag_aliases_without_accepting_extra_dashes() {
        assert_eq!(canonical_flag_name("r").as_deref(), Some("-r"));
        assert_eq!(
            canonical_flag_name("perf_basic-prof").as_deref(),
            Some("--perf-basic-prof")
        );
        assert_eq!(
            canonical_flag_name("--stack-trace-limit=-=value").as_deref(),
            Some("--stack-trace-limit")
        );
        assert_eq!(canonical_flag_name("---inspect-brk"), None);
    }
}
