// distinct objects on every evaluation.
function freshObjects(n) {
  let distinctFns = 0;
  let distinctRes = 0;
  let matched = 0;
  let prevFn = null;
  let prevRe = null;
  for (let i = 0; i < n; i++) {
    const f = function (x) { return x + 1; };
    const re = /a(b+)c/g;
    if (f !== prevFn) distinctFns++;
    if (re !== prevRe) distinctRes++;
    if (re.lastIndex === 0 && re.test("xabbbc")) matched += f(re.lastIndex);
    prevFn = f;
    prevRe = re;
  }
  return [distinctFns, distinctRes, matched].join(":");
}
let fresh = "";
for (let round = 0; round < 2; round++) fresh = freshObjects(3000);
console.log(fresh);
