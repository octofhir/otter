// Constructions the optimizer cannot inline: eight distinct constructors
// through one `new` site, each storing one field.
const cs = [];
for (let i = 0; i < 8; i++) cs.push(new Function('a', `this.v${i} = a;`));
function run(n) {
  let s = 0;
  for (let r = 0; r < n; r++) {
    const o = new cs[r & 7](r);
    s = (s + (o === null ? 0 : 1)) | 0;
  }
  return s;
}
run(100000);
const t = performance.now();
const s = run(20000000);
console.log(s, ((performance.now() - t) * 1e6 / 20000000).toFixed(2), 'ns/construct');
