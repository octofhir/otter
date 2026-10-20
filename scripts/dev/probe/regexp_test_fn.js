var strs = ['pyvpx', 'pyvbx', 'bapyx', 'pyv%x', 'qyvpx'];
var re0 = /^ba/;
function run(n) {
  var s = 0;
  for (var i = 0; i < n; i++) s += re0.test(strs[i % 5]) ? 1 : 0;
  return s;
}
function exec(n) {
  var s = 0;
  for (var i = 0; i < n; i++) { var m = re0.exec(strs[i % 5]); if (m) s += m[0].length; }
  return s;
}
var t0 = performance.now();
var s = run(30000000);
var t1 = performance.now();
var e = exec(30000000);
console.log(s, (t1 - t0).toFixed(0), e, (performance.now() - t1).toFixed(0));
