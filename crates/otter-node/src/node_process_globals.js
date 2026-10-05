Object.defineProperty(globalThis, Symbol.toStringTag, { value: 'global', configurable: true });
Object.defineProperty(globalThis, 'global', { value: globalThis, writable: true, configurable: true });
if (typeof process === 'object' && process.finalization === undefined) {
const exitRegistry = [];
const beforeExitRegistry = [];
const invoke = (list, event) => {
for (const [ref, callback] of list.splice(0)) {
const held = ref.deref();
if (held !== undefined) callback.call(process, held, event);
}
};
const validate = (obj, callback) => {
if ((typeof obj !== 'object' && typeof obj !== 'function') || obj === null) {
const err = new TypeError('The "ref" argument must be of type object. Received ' + (obj === null ? 'null' : typeof obj));
err.code = 'ERR_INVALID_ARG_TYPE';
throw err;
}
if (typeof callback !== 'function') {
const err = new TypeError('The "callback" argument must be of type function. Received ' + typeof callback);
err.code = 'ERR_INVALID_ARG_TYPE';
throw err;
}
};
process.finalization = {
register(obj, callback) {
validate(obj, callback);
exitRegistry.push([new WeakRef(obj), callback]);
},
registerBeforeExit(obj, callback) {
validate(obj, callback);
beforeExitRegistry.push([new WeakRef(obj), callback]);
},
unregister(obj) {
for (const list of [exitRegistry, beforeExitRegistry]) {
for (let i = list.length - 1; i >= 0; i--) {
if (list[i][0].deref() === obj) list.splice(i, 1);
}
}
},
};
process.on('beforeExit', () => invoke(beforeExitRegistry, 'beforeExit'));
process.on('exit', () => invoke(exitRegistry, 'exit'));
}
if (typeof process === 'object' && process.ref === undefined) {
const refSymbol = Symbol.for('nodejs.ref');
const unrefSymbol = Symbol.for('nodejs.unref');
process.ref = (maybeRefable) => {
const fn = maybeRefable?.[refSymbol] ?? maybeRefable?.ref;
if (typeof fn === 'function') fn.call(maybeRefable);
};
process.unref = (maybeRefable) => {
const fn = maybeRefable?.[unrefSymbol] ?? maybeRefable?.unref;
if (typeof fn === 'function') fn.call(maybeRefable);
};
}
if (typeof process === 'object' && process.report === undefined) {
process.report = {
getReport() { return { header: {}, javascriptStack: {}, libuv: [], workers: [], environmentVariables: {}, sharedObjects: [] }; },
writeReport() { return ''; },
directory: '', filename: '',
compact: false, excludeNetwork: false, excludeEnv: false,
reportOnFatalError: false, reportOnSignal: false, reportOnUncaughtException: false,
signal: 'SIGUSR2',
};
}
