const re = /(\w+)@(\w+)\.(\w+)(z)?/;
const g = /(\d+)/g;
let out = [];
for (let i = 0; i < 2000; i++) {
  const s = "user" + i + "@host" + (i * 7) + ".org tail";
  const m = re.exec(s);
  if (m.index !== 0 || m.input !== s || m[1] !== "user" + i || m[4] !== undefined || m.length !== 5 || m.groups !== undefined) throw new Error("bad " + i);
  g.lastIndex = 0; let n = 0, x; while ((x = g.exec(s)) !== null) { n += x[0].length + x.index; }
  out.push(n);
}
const named = /(?<a>x)(?<b>y)?/.exec("zxq");
console.log(out.reduce((a, b) => a + b, 0), JSON.stringify(named), named.groups.a, RegExp.$1, RegExp.lastMatch);
