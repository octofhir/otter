// node:os module implementation
// Provides operating system-related utility methods
// Uses lazy loading via native ops for expensive operations

(function() {
    const osModule = {
        // Static values (computed at extension load - cheap)
        arch: () => globalThis.__os_arch,
        platform: () => globalThis.__os_platform,
        type: () => globalThis.__os_type,
        endianness: () => globalThis.__os_endianness,

        // Dynamic values (lazy loaded via native ops - expensive)
        hostname: () => __otter_os_hostname(),
        homedir: () => __otter_os_homedir(),
        tmpdir: () => __otter_os_tmpdir(),
        release: () => __otter_os_release(),
        version: () => __otter_os_version(),
        totalmem: () => __otter_os_totalmem(),
        freemem: () => __otter_os_freemem(),
        uptime: () => __otter_os_uptime(),
        cpus: () => __otter_os_cpus(),
        loadavg: () => __otter_os_loadavg(),
        userInfo: () => __otter_os_userinfo(),
        machine: () => __otter_os_machine(),

        // Constants
        EOL: globalThis.__os_eol,
        devNull: globalThis.__os_devnull,

        // Priority constants
        constants: {
            priority: {
                PRIORITY_LOW: 19,
                PRIORITY_BELOW_NORMAL: 10,
                PRIORITY_NORMAL: 0,
                PRIORITY_ABOVE_NORMAL: -7,
                PRIORITY_HIGH: -14,
                PRIORITY_HIGHEST: -20,
            },
            signals: {},
            errno: {},
        },

        // Network interfaces (stub for now)
        networkInterfaces: () => ({}),

        // Priority functions
        getPriority: (pid) => {
            if (globalThis.__os_getpriority) {
                return globalThis.__os_getpriority(pid || 0);
            }
            return 0;
        },
        setPriority: (pid, priority) => {
            if (globalThis.__os_setpriority) {
                globalThis.__os_setpriority(pid || 0, priority);
            }
        },
    };

    // Register with module system if available
    if (globalThis.__registerModule) {
        globalThis.__registerModule('os', osModule);
        globalThis.__registerModule('node:os', osModule);
    }

    globalThis.__osModule = osModule;
})();
