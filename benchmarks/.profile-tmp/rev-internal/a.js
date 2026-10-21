const tries = {
  req: () => require('internal/util'),
  reqNode: () => require('node:internal/util'),
  reqUnder: () => require('__otter_internal'),
  reqDotDot: () => require('internal/../internal/util'),
  reqSlash: () => require('internal//util'),
  reqUpper: () => require('Internal/util'),
  reqNodeUpper: () => require('NODE:internal/util'),
  reqNodeSlash: () => require('node:/internal/util'),
  reqNodeFs: () => typeof require('node:fs').readFileSync,
  resolve: () => require.resolve('internal/util'),
  resolveNode: () => require.resolve('node:internal/util'),
  createReq: () => require('module').createRequire(__filename)('internal/util'),
  createReqNode: () => require('node:module').createRequire(__filename)('node:internal/util'),
  createReqRel: () => require('module').createRequire('relative')('internal/util'),
  createReqEmpty: () => require('module').createRequire('')('internal/util'),
  modLoad: () => require('module')._load('internal/util'),
  modResolve: () => require('module')._resolveFilename('internal/util'),
  protoReq: () => require('module').prototype.require.call({}, 'internal/util'),
  modReq: () => module.require('internal/util'),
  getBuiltin: () => Object.keys(process.getBuiltinModule('internal/util')).length,
  cacheKeys: () => Object.keys(require.cache).filter(k => /internal|__/.test(k)).slice(0, 10),
  binding: () => Object.keys(process.binding('util')),
  linked: () => typeof process._linkedBinding,
  mainReq: () => require.main && require.main.require && require.main.require('internal/util'),
  modParentReq: () => module.parent,
  constructorReq: () => require.constructor('return typeof primordials')(),
  realm: () => require('internal/bootstrap/realm'),
};
for (const [k, f] of Object.entries(tries)) {
  try { const r = f(); console.log(k, 'OK', typeof r === 'object' && r !== null ? JSON.stringify(Object.keys(r).slice(0,5)) : String(r).slice(0,100)); }
  catch (e) { console.log(k, 'ERR', e.code, String(e.message).split('\n')[0]); }
}
