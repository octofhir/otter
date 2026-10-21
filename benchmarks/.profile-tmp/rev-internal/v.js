const vm = require('vm');
try { console.log('vm require', vm.runInThisContext('typeof require')); } catch (e) { console.log('vm err', e.message); }
try { console.log('binding', Object.keys(process.binding('natives')).length); } catch (e) { console.log('binding err', e.message); }
try { console.log('linked', process._linkedBinding('fs')); } catch (e) { console.log('linked err', e.message); }
