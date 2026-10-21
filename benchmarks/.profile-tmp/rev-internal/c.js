console.log('warm');
const r = require.cache['internal/bootstrap/realm'];
console.log('realm keys', r && Object.keys(r), r && r.exports && Object.getOwnPropertyNames(r.exports));
const ib = r && r.exports && r.exports.internalBinding;
if (typeof ib === 'function') { for (const n of ['fs','process_methods','spawn_sync','os']) { try { const b = ib(n); console.log(n, Object.keys(b).slice(0,12)); } catch (e) { console.log(n, 'err', e.message); } } }
const p = r && r.exports && r.exports.primordials;
console.log('primordials', typeof p);
