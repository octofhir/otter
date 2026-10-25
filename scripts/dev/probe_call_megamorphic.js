// Calls the optimizer cannot inline: eight distinct closures through one
// site, so every iteration is a real call and return.
const fs = [];
for (let i = 0; i < 8; i++) fs.push(new Function('a', `return a + ${i};`));
function run(n) {
  let s = 0;
  for (let r = 0; r < n; r++) s = (s + fs[r & 7](r)) | 0;
  return s;
}
run(100000);
const t = performance.now();
const s = run(50000000);
console.log(s, ((performance.now() - t) * 1e6 / 50000000).toFixed(2), 'ns/call');
