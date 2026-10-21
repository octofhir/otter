var strs = ['pyvpx', 'pyvbx', 'bapyx', 'pyv%x', 'qyvpx'];
var re0 = /^ba/;
var s = 0;
var t0 = performance.now();
for (var i = 0; i < 30000000; i++) s += re0.test(strs[i % 5]) ? 1 : 0;
console.log(s, (performance.now() - t0).toFixed(0));
