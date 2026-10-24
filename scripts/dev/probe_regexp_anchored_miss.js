// `Exec(/^ba/, s)` misses in a hot loop: the per-call cost of a direct exec.
function Exec(re, string) {
  var sum = 0;
  re.lastIndex = 0;
  var array = re.exec(string);
  if (array) for (var i = 0; i < array.length; i++) { var s = array[i]; if (s) sum += s.length; }
  return sum;
}
const re0 = /^ba/;
const s0 = [];
for (let i = 0; i < 1000; i++) s0.push('pyvpx' + i);
const t = performance.now();
let s = 0;
for (let r = 0; r < 20000; r++) for (let i = 0; i < 1000; i++) s += Exec(re0, s0[i]);
console.log((performance.now() - t).toFixed(1), 'ms', s);
