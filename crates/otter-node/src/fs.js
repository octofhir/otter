// node:fs module wrapper

(function() {
    'use strict';

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
        readFile: (...args) => readFile(...args),
        writeFile: (...args) => writeFile(...args),
        readdir: (...args) => readdir(...args),
        stat: (...args) => stat(...args),
        mkdir: (...args) => mkdir(...args),
        rm: (...args) => rm(...args),
        exists: (...args) => exists(...args),
        rename: (...args) => rename(...args),
        copyFile: (...args) => copyFile(...args),
    };
    fsPromises.default = fsPromises;

    const fsModule = {
        readFileSync: (...args) => readFileSync(...args),
        writeFileSync: (...args) => writeFileSync(...args),
        readdirSync: (...args) => readdirSync(...args),
        statSync: (...args) => statSync(...args),
        mkdirSync: (...args) => mkdirSync(...args),
        rmSync: (...args) => rmSync(...args),
        existsSync: (...args) => existsSync(...args),
        copyFileSync: (...args) => copyFileSync(...args),

        readFile: callbackify(readFile),
        writeFile: callbackify(writeFile),
        readdir: callbackify(readdir),
        stat: callbackify(stat),
        mkdir: callbackify(mkdir),
        rm: callbackify(rm),
        exists: callbackify(exists),
        rename: callbackify(rename),
        copyFile: callbackify(copyFile),

        promises: fsPromises,
    };
    fsModule.default = fsModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('fs', fsModule);
        globalThis.__registerModule('node:fs', fsModule);
        globalThis.__registerModule('node:fs/promises', fsPromises);
    }
})();
