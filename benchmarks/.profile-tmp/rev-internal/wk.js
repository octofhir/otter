try { require('internal/util'); postMessage('LEAK'); } catch (e) { postMessage('req ' + e.code); }
try { postMessage('gbm ' + Object.keys(process.getBuiltinModule('internal/bootstrap/realm'))); } catch (e) { postMessage('gbm ' + e.message); }
import('node:internal/util').then(() => postMessage('imp LEAK'), e => postMessage('imp ' + e.code));
