const fake = { exports: { primordials: null, internalBinding: () => { throw new Error('POISONED realm'); } }, loaded: true };
require.cache['internal/bootstrap/realm'] = fake;
try { require('node:string_decoder'); console.log('loaded normally'); } catch (e) { console.log('builtin used planted module:', e.message); }
