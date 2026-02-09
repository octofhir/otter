// Node.js path module for Otter VM

const path = {
    join(...args) {
        return __path_join(...args);
    },

    resolve(...args) {
        return __path_resolve(...args);
    },

    dirname(p) {
        return __path_dirname(p);
    },

    basename(p, ext) {
        return __path_basename(p, ext);
    },

    extname(p) {
        return __path_extname(p);
    },

    normalize(p) {
        return __path_normalize(p);
    },

    isAbsolute(p) {
        return __path_is_absolute(p);
    },

    parse(p) {
        return __path_parse(p);
    },

    format(pathObject) {
        return __path_format(pathObject);
    },

    relative(from, to) {
        return __path_relative(from, to);
    },

    get sep() {
        return __path_sep();
    },

    get delimiter() {
        return __path_delimiter();
    },

    // POSIX-specific path handling (same as default on Unix)
    posix: null, // Set below

    // Windows-specific path handling
    win32: null, // Set below
};

// Self-reference for posix/win32
path.posix = path;
path.win32 = path;

// Export for module system
export default path;
