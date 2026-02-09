// Node.js fs/promises module - ESM export wrapper

export async function readFile(path, options) {
    const encoding = typeof options === 'string' ? options : options?.encoding;
    return __fs_read_file(path, encoding);
}

export async function writeFile(path, data, options) {
    return __fs_write_file(path, data, options);
}

export async function stat(path) {
    const s = await __fs_stat(path);
    return {
        ...s,
        isFile: () => s.isFile,
        isDirectory: () => s.isDirectory,
        isSymbolicLink: () => s.isSymbolicLink,
    };
}

export async function readdir(path, options) {
    return __fs_readdir_sync(path, options);
}

export async function mkdir(path, options) {
    return __fs_mkdir_sync(path, options);
}

export async function rmdir(path, options) {
    return __fs_rmdir_sync(path, options);
}

export async function unlink(path) {
    return __fs_unlink_sync(path);
}

export async function access(path, mode) {
    const exists = __fs_exists_sync(path);
    if (!exists) {
        throw new Error(`ENOENT: no such file or directory, access '${path}'`);
    }
}

export async function copyFile(src, dest, mode) {
    const data = await readFile(src);
    return writeFile(dest, data);
}

export async function rename(oldPath, newPath) {
    const data = await readFile(oldPath);
    await writeFile(newPath, data);
    await unlink(oldPath);
}

export async function realpath(path) {
    return __path_resolve(path);
}

export default {
    readFile,
    writeFile,
    stat,
    readdir,
    mkdir,
    rmdir,
    unlink,
    access,
    copyFile,
    rename,
    realpath,
};
