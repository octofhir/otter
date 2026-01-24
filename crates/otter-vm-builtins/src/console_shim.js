// Minimal console shim for the VM compiler subset.
// This intentionally avoids advanced syntax so it can run early and reliably.

globalThis.console = {
    log: __console_log,
    error: __console_error,
    warn: __console_warn,
    info: __console_info,
    debug: __console_debug,
    trace: __console_trace,
    time: __console_time,
    timeEnd: __console_timeEnd,
    timeLog: __console_timeLog,
    assert: __console_assert,
    clear: __console_clear,
    count: __console_count,
    countReset: __console_countReset,
    table: __console_table,
    dir: __console_dir,
    dirxml: __console_dirxml,
    group: __console_log,
    groupCollapsed: __console_log,
    groupEnd: __console_log,
};

