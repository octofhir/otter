// Node.js fs/promises module for Otter VM
// Provides Promise-based file system operations

const fsPromises = {
    async readFile(path, options) {
        const encoding = typeof options === 'string' ? options : options?.encoding;
        return __fs_read_file(path, encoding);
    },

    async writeFile(path, data, options) {
        return __fs_write_file(path, data, options);
    },

    async stat(path) {
        const stat = await __fs_stat(path);
        return {
            ...stat,
            isFile: () => stat.isFile,
            isDirectory: () => stat.isDirectory,
            isSymbolicLink: () => stat.isSymbolicLink,
        };
    },

    async readdir(path, options) {
        return __fs_readdir_sync(path, options);
    },

    async mkdir(path, options) {
        return __fs_mkdir_sync(path, options);
    },

    async rmdir(path, options) {
        return __fs_rmdir_sync(path, options);
    },

    async rm(path, options) {
        const stat = await fsPromises.stat(path).catch(() => null);
        if (!stat) return;

        if (stat.isDirectory()) {
            return fsPromises.rmdir(path, options);
        } else {
            return __fs_unlink_sync(path);
        }
    },

    async unlink(path) {
        return __fs_unlink_sync(path);
    },

    async access(path, mode) {
        const exists = __fs_exists_sync(path);
        if (!exists) {
            throw new Error(`ENOENT: no such file or directory, access '${path}'`);
        }
    },

    async copyFile(src, dest, mode) {
        const data = await fsPromises.readFile(src);
        return fsPromises.writeFile(dest, data);
    },

    async rename(oldPath, newPath) {
        // Read, write to new location, delete old
        const data = await fsPromises.readFile(oldPath);
        await fsPromises.writeFile(newPath, data);
        await fsPromises.unlink(oldPath);
    },

    async realpath(path) {
        return __path_resolve(path);
    }
};

// Export for module system
if (typeof module !== 'undefined') {
    module.exports = fsPromises;
}
