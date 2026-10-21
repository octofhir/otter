function b(n) { var s = 0; for (var i = 0; i < n; i++) s += Object.is(i, 5) ? 1 : 0; return s; }
b(100000); var t0 = performance.now(); b(60000000); console.log(((performance.now() - t0) / 60).toFixed(1), 'ns');
