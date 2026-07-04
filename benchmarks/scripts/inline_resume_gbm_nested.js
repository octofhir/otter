// Differential fixture: a guards-before-mutations nested body inlined inside a
// non-GBM enclosing method.
//
// `N.step` has no store before its deopt-able guard, so on its own it would
// re-run its call on a deopt. But `M.run` stores `this.c` before tail-calling
// `N.step`, so re-running the call would re-apply that store (double-counting
// `c`). The nested body must instead resume its own frame. The float `d` deopts
// `N.step` every call; `c` must stay exactly one-per-call, so a regression to
// the re-run path shows up as a changed checksum (and a doubled `c`).

function N(x) {
  this.x = x;
}
N.prototype.step = function (d) {
  var y = d + 1; // guard, NO store before it => N is GBM on its own
  if (y > 2000000000) return 1;
  return this.x + 1;
};

function M(n) {
  this.n = n;
  this.c = 0;
}
M.prototype.run = function (d) {
  this.c = this.c + 1; // store before the call => M non-GBM
  return this.n.step(d); // tail call to the GBM nested body
};

function caller(m, d) {
  return m.run(d);
}

var checksum = 0;
for (var k = 0; k < 200000; k++) {
  checksum += caller(new M(new N((k & 63) + 5)), 1); // warm: int d
}
var m2 = new M(new N(9));
checksum += caller(m2, 1.5); // float d => nested GBM guard resumes (must not re-run)
checksum += m2.c; // c must be exactly 1, not 2

console.log("inline_resume_gbm_nested=" + checksum);
