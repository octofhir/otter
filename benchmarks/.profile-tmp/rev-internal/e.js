const ib = process.getBuiltinModule('internal/bootstrap/realm').internalBinding;
const fsb = ib('fs');
try { console.log('utf8', String(fsb.readFileUtf8('/etc/hosts', 0)).length); } catch (e) { console.log('readFileUtf8 err', e.code, e.message); }
try { console.log('fd', fsb.open('/etc/hosts', 0, 0o666)); } catch (e) { console.log('open err', e.code, e.message); }
