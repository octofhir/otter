/**
 * path - Node.js compatible path module using #[dive] architecture.
 *
 * Native functions are defined in Rust with #[dive(swift)],
 * JS wrapper is in a separate file - clean separation of concerns.
 */
(function() {
    'use strict';

    // The native functions are exposed as path_* by the extension system
    const pathModule = {
        /**
         * Join path segments with the platform-specific separator.
         * @param {...string} paths - Path segments to join
         * @returns {string} Joined path
         */
        join: (...args) => path_join(args),

        /**
         * Resolve a sequence of paths to an absolute path.
         * @param {...string} paths - Paths to resolve
         * @returns {string} Absolute path
         */
        resolve: (...args) => path_resolve(args),

        /**
         * Get the directory name of a path.
         * @param {string} path - Input path
         * @returns {string} Directory name
         */
        dirname: (p) => path_dirname(p),

        /**
         * Get the last portion of a path.
         * @param {string} path - Input path
         * @param {string} [suffix] - Suffix to remove
         * @returns {string} Base name
         */
        basename: (p, suffix) => path_basename(p, suffix ?? null),

        /**
         * Get the extension of a path.
         * @param {string} path - Input path
         * @returns {string} Extension (including dot)
         */
        extname: (p) => path_extname(p),

        /**
         * Check if a path is absolute.
         * @param {string} path - Input path
         * @returns {boolean} True if absolute
         */
        isAbsolute: (p) => path_is_absolute(p),

        /**
         * Normalize a path.
         * @param {string} path - Input path
         * @returns {string} Normalized path
         */
        normalize: (p) => path_normalize(p),

        /**
         * Get relative path from 'from' to 'to'.
         * @param {string} from - Starting path
         * @param {string} to - Target path
         * @returns {string} Relative path
         */
        relative: (from, to) => path_relative(from, to),

        /**
         * Parse a path into components.
         * @param {string} path - Input path
         * @returns {{root: string, dir: string, base: string, ext: string, name: string}}
         */
        parse: (p) => path_parse(p),

        /**
         * Format a path from components.
         * @param {{root?: string, dir?: string, base?: string, ext?: string, name?: string}} pathObject
         * @returns {string} Formatted path
         */
        format: (obj) => path_format(obj || {}),

        /**
         * Platform-specific path separator.
         * @type {string}
         */
        get sep() { return path_sep(); },

        /**
         * Platform-specific path delimiter.
         * @type {string}
         */
        get delimiter() { return path_delimiter(); },
    };

    // Add default export
    pathModule.default = pathModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('path', pathModule);
    }
})();
