// Differential fixture for a two-level (nested) inline resume.
//
// `M.run` is inlined into `caller`; its `return this.n.step(d)` is a tail call
// to `N.step`, which the optimizing tier recursively splices in. Both bodies
// store `this` before a guard that can deopt (the int32 `d + 1` in `N.step`),
// so both are non-GBM: a guard inside the nested `N.step` cannot re-run the
// call and instead resumes a two-frame interpreter stack — `M.run` at the point
// just past the nested call, `N.step` at the failing guard. A long warmup with
// an int `d` tiers `caller` up with the whole chain spliced in; the float `d`
// then deopts `N.step` on every call, firing the multi-frame resume. Every tier
// must agree byte-for-byte, so a regression in the nested resume chain (frame
// order, return-register wiring, register windows) changes the checksum.

function N(x) {
  this.x = x;
}
N.prototype.step = function (d) {
  this.x = this.x - 1; // store => N non-GBM
  var y = d + 1; // int32 guard AFTER the store; deopts on a float d
  if (y > 2000000000) this.x = 0; // keep y live
  if (this.x < 0) return 100; // forward branch
  return this.x;
};

function M(n) {
  this.n = n;
  this.c = 0;
}
M.prototype.run = function (d) {
  this.c = this.c + 1; // store => M non-GBM
  return this.n.step(d); // tail call => N.step recursively inlined
};

function caller(m, d) {
  return m.run(d);
}

var checksum = 0;
for (var k = 0; k < 80000; k++) {
  checksum += caller(new M(new N((k & 63) + 5)), 1); // warm: int d
}
checksum += caller(new M(new N(9)), 1.5); // float d => two-frame resume

console.log("inline_resume_nested=" + checksum);
