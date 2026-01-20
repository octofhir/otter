// node:fs module wrapper

(function() {
    'use strict';

    // Convert URL objects to file paths (Node.js compatibility)
    function toPath(pathOrUrl) {
        if (pathOrUrl == null) return pathOrUrl;

        // Handle URL objects (both native URL and file:// strings)
        if (typeof pathOrUrl === 'object' && pathOrUrl.href) {
            const href = pathOrUrl.href;
            if (href.startsWith('file://')) {
                // Extract path from file:// URL
                // file:///path/to/file -> /path/to/file
                return href.slice(7); // Remove 'file://'
            }
            return href;
        }

        // Handle file:// URL strings
        if (typeof pathOrUrl === 'string' && pathOrUrl.startsWith('file://')) {
            return pathOrUrl.slice(7);
        }

        return pathOrUrl;
    }

    function callbackify(promiseFn) {
        return function(...args) {
            const cb = args.length && typeof args[args.length - 1] === 'function'
                ? args.pop()
                : null;

            if (!cb) return promiseFn(...args);

            promiseFn(...args).then(
                (value) => cb(null, value),
                (err) => cb(err)
            );
        };
    }

    const fsPromises = {
        readFile: (path, ...args) => readFile(toPath(path), ...args),
        writeFile: (path, ...args) => writeFile(toPath(path), ...args),
        readdir: (path, ...args) => readdir(toPath(path), ...args),
        stat: (path, ...args) => stat(toPath(path), ...args),
        mkdir: (path, ...args) => mkdir(toPath(path), ...args),
        rm: (path, ...args) => rm(toPath(path), ...args),
        unlink: (path, ...args) => unlink(toPath(path), ...args),
        exists: (path, ...args) => exists(toPath(path), ...args),
        rename: (oldPath, newPath, ...args) => rename(toPath(oldPath), toPath(newPath), ...args),
        copyFile: (src, dest, ...args) => copyFile(toPath(src), toPath(dest), ...args),
    };
    fsPromises.default = fsPromises;

    const fsModule = {
        readFileSync: (path, ...args) => readFileSync(toPath(path), ...args),
        writeFileSync: (path, ...args) => writeFileSync(toPath(path), ...args),
        readdirSync: (path, ...args) => readdirSync(toPath(path), ...args),
        statSync: (path, ...args) => statSync(toPath(path), ...args),
        mkdirSync: (path, ...args) => mkdirSync(toPath(path), ...args),
        rmSync: (path, ...args) => rmSync(toPath(path), ...args),
        unlinkSync: (path, ...args) => unlinkSync(toPath(path), ...args),
        existsSync: (path, ...args) => existsSync(toPath(path), ...args),
        copyFileSync: (src, dest, ...args) => copyFileSync(toPath(src), toPath(dest), ...args),

        readFile: callbackify((path, ...args) => readFile(toPath(path), ...args)),
        writeFile: callbackify((path, ...args) => writeFile(toPath(path), ...args)),
        readdir: callbackify((path, ...args) => readdir(toPath(path), ...args)),
        stat: callbackify((path, ...args) => stat(toPath(path), ...args)),
        mkdir: callbackify((path, ...args) => mkdir(toPath(path), ...args)),
        rm: callbackify((path, ...args) => rm(toPath(path), ...args)),
        unlink: callbackify((path, ...args) => unlink(toPath(path), ...args)),
        exists: callbackify((path, ...args) => exists(toPath(path), ...args)),
        rename: callbackify((oldPath, newPath, ...args) => rename(toPath(oldPath), toPath(newPath), ...args)),
        copyFile: callbackify((src, dest, ...args) => copyFile(toPath(src), toPath(dest), ...args)),

        promises: fsPromises,
    };
    fsModule.default = fsModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('fs', fsModule);
        globalThis.__registerNodeBuiltin('fs/promises', fsPromises);
    }
})();
