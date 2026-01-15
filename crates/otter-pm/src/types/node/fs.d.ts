/**
 * The `node:fs` module enables interacting with the file system.
 * All file system operations have synchronous and promise-based forms.
 * @module node:fs
 */
declare module "node:fs" {
    import { Buffer } from "node:buffer";

    /**
     * File system statistics returned by stat operations.
     */
    export interface Stats {
        /** True if this is a regular file */
        isFile: boolean;
        /** True if this is a directory */
        isDirectory: boolean;
        /** True if this is a symbolic link */
        isSymbolicLink: boolean;
        /** Size of the file in bytes */
        size: number;
        /** File mode (permissions) as a numeric value */
        mode: number;
        /** Last modified time in milliseconds since Unix epoch */
        mtimeMs?: number;
        /** Last access time in milliseconds since Unix epoch */
        atimeMs?: number;
        /** Creation time in milliseconds since Unix epoch */
        ctimeMs?: number;
    }

    /**
     * Options for mkdir operations.
     */
    export interface MkdirOptions {
        /** Create parent directories if they don't exist (default: false) */
        recursive?: boolean;
        /** Directory mode (default: 0o777) */
        mode?: number;
    }

    /**
     * Options for rm operations.
     */
    export interface RmOptions {
        /** Recursively remove directories (default: false) */
        recursive?: boolean;
        /** Ignore errors if path doesn't exist (default: false) */
        force?: boolean;
    }

    // ============================================================================
    // Synchronous Methods (node:fs style)
    // ============================================================================

    /**
     * Synchronously read the entire contents of a file.
     * @param path Path to the file
     * @param encoding If specified, returns a string; otherwise returns a Buffer
     * @returns File contents as string (if encoding specified) or Buffer
     * @throws Error if file cannot be read or permission denied
     * @example
     * const data = fs.readFileSync('/path/to/file.txt', 'utf8');
     */
    export function readFileSync(path: string, encoding: "utf8" | "utf-8"): string;
    export function readFileSync(path: string, encoding?: null): Buffer;
    export function readFileSync(path: string, options?: { encoding?: string | null }): string | Buffer;

    /**
     * Synchronously write data to a file, replacing if it already exists.
     * @param path Path to the file
     * @param data Data to write
     * @throws Error if file cannot be written or permission denied
     * @example
     * fs.writeFileSync('/path/to/file.txt', 'Hello World');
     */
    export function writeFileSync(path: string, data: string | Buffer | Uint8Array): void;

    /**
     * Synchronously read the contents of a directory.
     * @param path Path to the directory
     * @returns Array of file and directory names in the directory
     * @throws Error if directory cannot be read or permission denied
     * @example
     * const files = fs.readdirSync('/path/to/dir');
     */
    export function readdirSync(path: string): string[];

    /**
     * Synchronously get file status.
     * @param path Path to the file or directory
     * @returns Stats object describing the file
     * @throws Error if path cannot be accessed or permission denied
     * @example
     * const stats = fs.statSync('/path/to/file.txt');
     * if (stats.isFile) { ... }
     */
    export function statSync(path: string): Stats;

    /**
     * Synchronously create a directory.
     * @param path Path for the new directory
     * @param options Options including recursive creation
     * @throws Error if directory cannot be created or permission denied
     * @example
     * fs.mkdirSync('/path/to/new/dir', { recursive: true });
     */
    export function mkdirSync(path: string, options?: MkdirOptions): void;

    /**
     * Synchronously remove a file or directory.
     * @param path Path to remove
     * @param options Options including recursive removal
     * @throws Error if path cannot be removed or permission denied
     * @example
     * fs.rmSync('/path/to/file.txt');
     * fs.rmSync('/path/to/dir', { recursive: true });
     */
    export function rmSync(path: string, options?: RmOptions): void;

    /**
     * Synchronously check if a path exists.
     * @param path Path to check
     * @returns True if path exists
     * @example
     * if (fs.existsSync('/path/to/file.txt')) { ... }
     */
    export function existsSync(path: string): boolean;

    /**
     * Synchronously copy a file.
     * @param src Source file path
     * @param dest Destination file path
     * @returns Number of bytes copied
     * @throws Error if file cannot be copied or permission denied
     * @example
     * fs.copyFileSync('/path/to/src.txt', '/path/to/dest.txt');
     */
    export function copyFileSync(src: string, dest: string): number;

    // ============================================================================
    // Re-export promises namespace
    // ============================================================================

    export * as promises from "node:fs/promises";
}

/**
 * The `node:fs/promises` module provides Promise-based file system methods.
 * @module node:fs/promises
 */
declare module "node:fs/promises" {
    import { Buffer } from "node:buffer";
    import { Stats, MkdirOptions, RmOptions } from "node:fs";

    /**
     * Asynchronously read the entire contents of a file.
     * @param path Path to the file
     * @param encoding If specified, returns a string; otherwise returns a Buffer
     * @returns Promise resolving to file contents
     * @throws Error if file cannot be read or permission denied
     * @example
     * const data = await fs.readFile('/path/to/file.txt', 'utf8');
     */
    export function readFile(path: string, encoding: "utf8" | "utf-8"): Promise<string>;
    export function readFile(path: string, encoding?: null): Promise<Buffer>;
    export function readFile(path: string, options?: { encoding?: string | null }): Promise<string | Buffer>;

    /**
     * Asynchronously write data to a file, replacing if it already exists.
     * @param path Path to the file
     * @param data Data to write
     * @returns Promise that resolves when write is complete
     * @throws Error if file cannot be written or permission denied
     * @example
     * await fs.writeFile('/path/to/file.txt', 'Hello World');
     */
    export function writeFile(path: string, data: string | Buffer | Uint8Array): Promise<void>;

    /**
     * Asynchronously read the contents of a directory.
     * @param path Path to the directory
     * @returns Promise resolving to array of file names
     * @throws Error if directory cannot be read or permission denied
     * @example
     * const files = await fs.readdir('/path/to/dir');
     */
    export function readdir(path: string): Promise<string[]>;

    /**
     * Asynchronously get file status.
     * @param path Path to the file or directory
     * @returns Promise resolving to Stats object
     * @throws Error if path cannot be accessed or permission denied
     * @example
     * const stats = await fs.stat('/path/to/file.txt');
     */
    export function stat(path: string): Promise<Stats>;

    /**
     * Asynchronously create a directory.
     * @param path Path for the new directory
     * @param options Options including recursive creation
     * @returns Promise that resolves when directory is created
     * @throws Error if directory cannot be created or permission denied
     * @example
     * await fs.mkdir('/path/to/new/dir', { recursive: true });
     */
    export function mkdir(path: string, options?: MkdirOptions): Promise<void>;

    /**
     * Asynchronously remove a file or directory.
     * @param path Path to remove
     * @param options Options including recursive removal
     * @returns Promise that resolves when removal is complete
     * @throws Error if path cannot be removed or permission denied
     * @example
     * await fs.rm('/path/to/file.txt');
     */
    export function rm(path: string, options?: RmOptions): Promise<void>;

    /**
     * Asynchronously check if a path exists.
     * @param path Path to check
     * @returns Promise resolving to true if path exists
     * @example
     * if (await fs.exists('/path/to/file.txt')) { ... }
     */
    export function exists(path: string): Promise<boolean>;

    /**
     * Asynchronously rename/move a file or directory.
     * @param oldPath Current path
     * @param newPath New path
     * @returns Promise that resolves when rename is complete
     * @throws Error if rename fails or permission denied
     * @example
     * await fs.rename('/path/to/old.txt', '/path/to/new.txt');
     */
    export function rename(oldPath: string, newPath: string): Promise<void>;

    /**
     * Asynchronously copy a file.
     * @param src Source file path
     * @param dest Destination file path
     * @returns Promise resolving to number of bytes copied
     * @throws Error if file cannot be copied or permission denied
     * @example
     * await fs.copyFile('/path/to/src.txt', '/path/to/dest.txt');
     */
    export function copyFile(src: string, dest: string): Promise<number>;
}

// Also support the 'fs' module (without node: prefix)
declare module "fs" {
    export * from "node:fs";
}

declare module "fs/promises" {
    export * from "node:fs/promises";
}
