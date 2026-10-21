const { Worker } = require('worker_threads');
const w = new Worker(`
const { parentPort } = require('worker_threads');
let out = [];
for (const s of ['internal/util','node:internal/util']) { try { require(s); out.push(s+':LEAK'); } catch (e) { out.push(s+':'+e.code); } }
try { out.push('gbm:' + Object.keys(process.getBuiltinModule('internal/bootstrap/realm'))); } catch (e) { out.push('gbm:' + e.message); }
parentPort.postMessage(out.join(' '));
`, { eval: true });
w.on('message', m => { console.log('worker:', m); });
w.on('error', e => console.log('werr', e.message));
const vm = require('vm');
try { console.log('vm require', vm.runInThisContext('typeof require')); } catch (e) { console.log('vm err', e.message); }
try { console.log('vm ctx', vm.runInNewContext('typeof process')); } catch (e) { console.log('vm err', e.message); }
try { console.log('binding', Object.keys(process.binding('natives')).length); } catch (e) { console.log('binding err', e.message); }
try { console.log('linked', process._linkedBinding('fs')); } catch (e) { console.log('linked err', e.message); }
