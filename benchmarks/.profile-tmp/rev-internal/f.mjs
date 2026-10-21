const specs = ['internal/util','node:internal/util','__otter_internal','internal/bootstrap/realm','node:internal/bootstrap/realm','internal/../internal/util','node:internal/../fs','node:fs/../internal/util','Internal/util','file:internal/util','node:%69nternal/util','node:internal%2Futil','node:internal/util?x','node:internal/util#x','node:_http_agent','_http_agent','node:test/reporters','node:internal/console/constructor'];
for (const s of specs) {
  try { const m = await import(s); console.log(s, 'OK', Object.keys(m).slice(0,5)); }
  catch (e) { console.log(s, 'ERR', e.code, String(e.message).split('\n')[0]); }
}
console.log('meta', import.meta.resolve && import.meta.resolve('internal/util'));
