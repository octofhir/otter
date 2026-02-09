// Node.js path module - ESM export wrapper

export function join(...args) {
    return __path_join(...args);
}

export function resolve(...args) {
    return __path_resolve(...args);
}

export function dirname(p) {
    return __path_dirname(p);
}

export function basename(p, ext) {
    return __path_basename(p, ext);
}

export function extname(p) {
    return __path_extname(p);
}

export function normalize(p) {
    return __path_normalize(p);
}

export function isAbsolute(p) {
    return __path_is_absolute(p);
}

export function parse(p) {
    return __path_parse(p);
}

export function format(pathObject) {
    return __path_format(pathObject);
}

export function relative(from, to) {
    return __path_relative(from, to);
}

export const sep = __path_sep();
export const delimiter = __path_delimiter();

// POSIX/win32 self-reference
export const posix = { join, resolve, dirname, basename, extname, normalize, isAbsolute, parse, format, relative, sep, delimiter };
export const win32 = posix;

export default {
    join,
    resolve,
    dirname,
    basename,
    extname,
    normalize,
    isAbsolute,
    parse,
    format,
    relative,
    sep,
    delimiter,
    posix,
    win32,
};
