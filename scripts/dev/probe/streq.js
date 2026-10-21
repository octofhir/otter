function eq(a, b) { return a === b; }
function ne(a, b) { return a !== b; }
let o = { abc: 1, abd: 2 };
let keys = Object.keys(o);
let n = 0;
for (let i = 0; i < 200000; i++) {
  const x = "ab" + (i & 1 ? "c" : "d");
  const y = keys[i & 1];
  if (eq(x, y)) n++;
  if (eq(x, "abc")) n += 2;
  if (ne(keys[0], "abc")) n += 100;
  if (eq("é" + "x", "éx")) n += 3;
  if (eq("ab", "abc")) n += 1000;
}
console.log(n);
const lone = String.fromCharCode(0xD800);
const repl = "�";
const bag = {};
bag[lone] = 1;
bag[repl] = 2;
let m = 0;
for (let i = 0; i < 100000; i++) {
  const k = i & 1 ? lone : repl;
  if (eq(k, "�")) m++;
  if (eq(k, lone)) m += 10;
  if (eq(String.fromCharCode(0xFFFD), "�")) m += 100;
}
console.log(m, bag[lone], bag[repl]);
