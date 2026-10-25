// A RegExp literal evaluated in a hot loop: one fresh, escaping object per
// evaluation.
function make(i) { return /XUGZY[a-z]+/gi; }
const keep = new Array(1024);
let s = 0;
const t = performance.now();
for (let i = 0; i < 20000000; i++) { const re = make(i); keep[i & 1023] = re; s += re.lastIndex + 1; }
console.log(s, ((performance.now() - t) * 1e6 / 20000000).toFixed(1), 'ns/literal');
