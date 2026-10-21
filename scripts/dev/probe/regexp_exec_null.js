// Per-call cost of the Octane regexp `Exec` helper on an anchored pattern
// that fails at offset 0: the RegExp protocol around a trivial match.
function Exec(re, string) {
  var sum = 0;
  re.lastIndex = 0;
  var array = re.exec(string);
  if (array) {
    for (var i = 0; i < array.length; i++) {
      var substring = array[i];
      if (substring) sum += substring.length;
    }
  }
  return sum;
}
const re0 = /^ba/;
const inputs = [];
for (let i = 0; i < 64; i++) inputs.push('pyvpx' + String.fromCharCode(97 + (i % 26)));
const n = 2000000;
let sum = 0;
const t0 = performance.now();
for (let i = 0; i < n; i++) sum += Exec(re0, inputs[i & 63]);
const t1 = performance.now();
let hits = 0;
const t2 = performance.now();
for (let i = 0; i < n; i++) if (re0.test(inputs[i & 63])) hits++;
const t3 = performance.now();
console.log(JSON.stringify({ execNs: Math.round(((t1 - t0) * 1e6) / n), testNs: Math.round(((t3 - t2) * 1e6) / n), sum, hits }));
