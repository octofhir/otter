// `Exec(/^ba/, s)` over the Octane regexp fixture's input shape: 6511
// distinct strings built by substring concatenation, each searched many
// times, with other RegExps sharing the call site.
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
let seed = 49734321;
function random() {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  return seed / 0x80000000;
}
function computeInputVariants(str, n) {
  var variants = [str];
  for (var i = 1; i < n; i++) {
    var pos = Math.floor(random() * str.length);
    var chr = String.fromCharCode((str.charCodeAt(pos) + Math.floor(random() * 128)) % 128);
    variants[i] = str.substring(0, pos) + chr + str.substring(pos + 1, str.length);
  }
  return variants;
}
const re0 = /^ba/;
const re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
const s0 = computeInputVariants('pyvpx', 6511);
const s1 = computeInputVariants('uggc://jjj.snprobbx.pbz/ybtva.cuc', 1844);
const rounds = Number(process.argv[2] || 100);
let sum = 0;
let t = performance.now();
for (let r = 0; r < rounds; r++) for (let i = 0; i < s0.length; i++) sum += Exec(re0, s0[i]);
const re0Ns = ((performance.now() - t) * 1e6) / (rounds * s0.length);
t = performance.now();
for (let r = 0; r < rounds; r++) for (let i = 0; i < s1.length; i++) sum += Exec(re1, s1[i]);
const re1Ns = ((performance.now() - t) * 1e6) / (rounds * s1.length);
console.log(JSON.stringify({ re0Ns: Math.round(re0Ns), re1Ns: Math.round(re1Ns), sum }));
