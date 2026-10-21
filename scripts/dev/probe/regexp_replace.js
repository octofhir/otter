var re = /\./g;
var trim = /^\s*|\s*$/g;
function r(n) { var s = 0; for (var i = 0; i < n; i++) s += 'a.b.c.d.e'.replace(re, '_').length; return s; }
function t(n) { var s = 0; for (var i = 0; i < n; i++) s += 'QBZPbageby_cynprubyqre'.replace(trim, '').length; return s; }
var t0 = performance.now(); r(5000000); var t1 = performance.now(); t(5000000); console.log((t1 - t0).toFixed(0), (performance.now() - t1).toFixed(0));
