// Node.js os module - ESM export wrapper

export function platform() {
    return __process_platform();
}

export function arch() {
    return __process_arch();
}

export function cpus() {
    // Stub - would need native support
    return [];
}

export function freemem() {
    return 0;
}

export function totalmem() {
    return 0;
}

export function homedir() {
    return __env_get('HOME') || '/';
}

export function tmpdir() {
    return __env_get('TMPDIR') || '/tmp';
}

export function hostname() {
    return 'localhost';
}

export function type() {
    const p = platform();
    if (p === 'darwin') return 'Darwin';
    if (p === 'linux') return 'Linux';
    if (p === 'win32') return 'Windows_NT';
    return p;
}

export function release() {
    return '';
}

export function uptime() {
    return 0;
}

export function userInfo() {
    return {
        uid: -1,
        gid: -1,
        username: __env_get('USER') || '',
        homedir: homedir(),
        shell: __env_get('SHELL') || '',
    };
}

export function networkInterfaces() {
    return {};
}

export const EOL = platform() === 'win32' ? '\r\n' : '\n';

export const constants = {
    signals: {},
    errno: {},
};

export default {
    platform,
    arch,
    cpus,
    freemem,
    totalmem,
    homedir,
    tmpdir,
    hostname,
    type,
    release,
    uptime,
    userInfo,
    networkInterfaces,
    EOL,
    constants,
};
