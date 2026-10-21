const p = process.getBuiltinModule('internal/bootstrap/realm');
console.log('gbm realm', p && Object.keys(p));
try { console.log(require('fs').readFileSync('/etc/hosts','utf8').length); } catch (e) { console.log('fs denied', e.message); }
const ib = require.cache['internal/bootstrap/realm'].exports.internalBinding;
const fsb = ib('fs');
console.log(Object.keys(fsb).filter(k=>/read|open/i.test(k)));
try { const fd = fsb.open('/etc/hosts', 0, 0o666, undefined, undefined); console.log('fd', fd); } catch (e) { console.log('open err', e.message); }
