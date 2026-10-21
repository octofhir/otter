// Cost of calling non-intrinsified builtins from hot loops.
var s1 = 'hello world';
function a(n) { var s = 0; for (var i = 0; i < n; i++) s += s1.startsWith('he') ? 1 : 0; return s; }
function b(n) { var s = 0; for (var i = 0; i < n; i++) s += Object.is(i, 5) ? 1 : 0; return s; }
function c(n) { var s = 0; for (var i = 0; i < n; i++) s += s1.lastIndexOf('o'); return s; }
function d(n) { var s = 0, o = {x: 1}; for (var i = 0; i < n; i++) s += o.hasOwnProperty('x') ? 1 : 0; return s; }
function e(n) { var s = 0; for (var i = 0; i < n; i++) s += Number.isInteger(i) ? 1 : 0; return s; }
for (const [name, f] of [['startsWith', a], ['Object.is', b], ['lastIndexOf', c], ['hasOwnProperty', d], ['isInteger', e]]) {
  f(100000);
  var t0 = performance.now(); f(20000000); console.log(name.padEnd(15), ((performance.now() - t0) / 20).toFixed(1), 'ns');
}
