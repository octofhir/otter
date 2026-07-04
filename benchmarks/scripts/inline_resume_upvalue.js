// Differential fixture for an inline resume whose body reads an upvalue.
//
// `C.m` stores `this.x` before a guard that can deopt (the int32 `d + 1`) and
// then reads the module-level `BASE` through its closure — so a resume of this
// non-GBM body must rebuild the frame with the method closure's real upvalue
// spine, not an empty one. A long warmup with an int `d` tiers `run` up with
// `C.m` inlined; the float `d` then deopts every call, firing the resume, which
// reads `BASE` from the reconstructed spine. Every tier must agree, so an empty
// or wrong resume spine changes the checksum.

var BASE = 1000;

function C(x) {
  this.x = x;
}
C.prototype.m = function (d) {
  this.x = this.x - 1; // store => non-GBM
  var y = d + 1; // int32 guard AFTER the store; deopts on a float d
  if (y > 2000000000) this.x = 0; // keep y live
  return this.x + BASE; // reads BASE (upvalue) after the store
};

function run(c, d) {
  return c.m(d);
}

var checksum = 0;
for (var k = 0; k < 200000; k++) {
  checksum += run(new C((k & 63) + 5), 1); // warm: int d
}
checksum += run(new C(9), 1.5); // float d => resume reads the upvalue

console.log("inline_resume_upvalue=" + checksum);
