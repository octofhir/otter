/**
 * The `node:os` module provides operating system-related utility methods and properties.
 * @module node:os
 */
declare module "node:os" {
    /**
     * CPU core information.
     */
    export interface CpuInfo {
        /** CPU model name */
        model: string;
        /** CPU speed in MHz */
        speed: number;
        /** CPU time statistics */
        times: {
            /** Time spent in user mode (ms) */
            user: number;
            /** Time spent in nice mode (ms) */
            nice: number;
            /** Time spent in system mode (ms) */
            sys: number;
            /** Time spent idle (ms) */
            idle: number;
            /** Time spent in IRQ mode (ms) */
            irq: number;
        };
    }

    /**
     * Network interface information.
     */
    export interface NetworkInterfaceInfo {
        /** Network interface address */
        address: string;
        /** Network mask */
        netmask: string;
        /** Address family ('IPv4' or 'IPv6') */
        family: "IPv4" | "IPv6";
        /** MAC address */
        mac: string;
        /** Whether this is an internal interface */
        internal: boolean;
        /** CIDR notation */
        cidr: string | null;
    }

    /**
     * User information.
     */
    export interface UserInfo {
        /** User ID (Unix only) */
        uid: number;
        /** Group ID (Unix only) */
        gid: number;
        /** Username */
        username: string;
        /** Home directory */
        homedir: string;
        /** Default shell (Unix only) */
        shell: string | null;
    }

    /**
     * Returns the operating system CPU architecture.
     * Possible values: 'x64', 'arm64', 'ia32', 'arm'
     *
     * @example
     * ```ts
     * console.log(os.arch()); // 'arm64'
     * ```
     */
    export function arch(): "x64" | "arm64" | "ia32" | "arm" | string;

    /**
     * Returns an array of objects containing information about each CPU/core.
     *
     * @example
     * ```ts
     * const cpus = os.cpus();
     * console.log(`${cpus.length} cores`);
     * console.log(cpus[0].model); // 'Apple M1 Pro'
     * ```
     */
    export function cpus(): CpuInfo[];

    /**
     * Returns 'BE' for big endian or 'LE' for little endian.
     */
    export function endianness(): "BE" | "LE";

    /**
     * Returns the amount of free system memory in bytes.
     *
     * @example
     * ```ts
     * const freeGB = os.freemem() / (1024 ** 3);
     * console.log(`Free memory: ${freeGB.toFixed(2)} GB`);
     * ```
     */
    export function freemem(): number;

    /**
     * Returns the string path of the current user's home directory.
     *
     * @example
     * ```ts
     * console.log(os.homedir()); // '/Users/username'
     * ```
     */
    export function homedir(): string;

    /**
     * Returns the hostname of the operating system.
     *
     * @example
     * ```ts
     * console.log(os.hostname()); // 'macbook-pro.local'
     * ```
     */
    export function hostname(): string;

    /**
     * Returns an array containing the 1, 5, and 15 minute load averages.
     * Only available on Unix. Returns [0, 0, 0] on Windows.
     *
     * @example
     * ```ts
     * const [load1, load5, load15] = os.loadavg();
     * console.log(`Load average: ${load1.toFixed(2)}`);
     * ```
     */
    export function loadavg(): [number, number, number];

    /**
     * Returns the machine type as a string (e.g., 'arm64', 'x86_64').
     */
    export function machine(): string;

    /**
     * Returns an object containing network interfaces.
     * Currently returns an empty object in Otter.
     */
    export function networkInterfaces(): Record<string, NetworkInterfaceInfo[]>;

    /**
     * Returns the operating system platform.
     * Possible values: 'darwin', 'linux', 'win32'
     *
     * @example
     * ```ts
     * console.log(os.platform()); // 'darwin'
     * ```
     */
    export function platform(): "darwin" | "linux" | "win32" | string;

    /**
     * Returns the operating system release version.
     *
     * @example
     * ```ts
     * console.log(os.release()); // '14.0'
     * ```
     */
    export function release(): string;

    /**
     * Returns the operating system's default directory for temporary files.
     *
     * @example
     * ```ts
     * console.log(os.tmpdir()); // '/tmp'
     * ```
     */
    export function tmpdir(): string;

    /**
     * Returns the total amount of system memory in bytes.
     *
     * @example
     * ```ts
     * const totalGB = os.totalmem() / (1024 ** 3);
     * console.log(`Total memory: ${totalGB.toFixed(2)} GB`);
     * ```
     */
    export function totalmem(): number;

    /**
     * Returns the operating system type.
     * Possible values: 'Darwin', 'Linux', 'Windows_NT'
     *
     * @example
     * ```ts
     * console.log(os.type()); // 'Darwin'
     * ```
     */
    export function type(): "Darwin" | "Linux" | "Windows_NT" | string;

    /**
     * Returns the system uptime in seconds.
     *
     * @example
     * ```ts
     * const uptimeHours = os.uptime() / 3600;
     * console.log(`Uptime: ${uptimeHours.toFixed(1)} hours`);
     * ```
     */
    export function uptime(): number;

    /**
     * Returns information about the currently effective user.
     *
     * @example
     * ```ts
     * const user = os.userInfo();
     * console.log(user.username); // 'john'
     * console.log(user.homedir);  // '/Users/john'
     * ```
     */
    export function userInfo(): UserInfo;

    /**
     * Returns a string identifying the kernel version.
     *
     * @example
     * ```ts
     * console.log(os.version()); // 'Darwin Kernel Version 23.0.0'
     * ```
     */
    export function version(): string;

    /**
     * Returns the scheduling priority for the process specified by pid.
     * @param pid Process ID (default: 0, meaning current process)
     */
    export function getPriority(pid?: number): number;

    /**
     * Sets the scheduling priority for the process specified by pid.
     * @param pid Process ID (default: 0, meaning current process)
     * @param priority The scheduling priority (-20 to 19)
     */
    export function setPriority(pid: number | undefined, priority: number): void;
    export function setPriority(priority: number): void;

    /**
     * The operating system-specific end-of-line marker.
     * - '\n' on POSIX
     * - '\r\n' on Windows
     */
    export const EOL: "\n" | "\r\n";

    /**
     * The platform-specific file path of the null device.
     * - '/dev/null' on POSIX
     * - '\\\\.\\nul' on Windows
     */
    export const devNull: "/dev/null" | "\\\\.\\nul";

    /**
     * Operating system constants.
     */
    export const constants: {
        /** Scheduling priority constants */
        priority: {
            PRIORITY_LOW: 19;
            PRIORITY_BELOW_NORMAL: 10;
            PRIORITY_NORMAL: 0;
            PRIORITY_ABOVE_NORMAL: -7;
            PRIORITY_HIGH: -14;
            PRIORITY_HIGHEST: -20;
        };
        /** Signal constants */
        signals: Record<string, number>;
        /** Error constants */
        errno: Record<string, number>;
    };
}

// Also support the 'os' module (without node: prefix)
declare module "os" {
    export * from "node:os";
}
