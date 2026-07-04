// Differential fixture for the non-GBM inline-resume deopt path.
//
// A method that stores `this.n` BEFORE a guard that can deopt (the int32
// `delta + 1`) is a non-GBM inlined body: the guard cannot re-run the call (it
// would double the store) and instead resumes the callee frame mid-execution.
// This path corrupted under OSR compiles — the inlined callee's registers live
// above the caller window, so OSR Param-seeding read frame slots the entry
// never populated — while function-entry compiles resume soundly. Both shapes
// run here; every tier (interp / jit / jit-osr) must agree byte-for-byte, so a
// regression in either resume frame state changes the checksum.

function Counter(start) {
  this.n = start;
}
Counter.prototype.tick = function (delta) {
  this.n = this.n - 1; // store before the guard => non-GBM
  var x = delta + 1; // int32 guard AFTER the store; deopts on a float delta
  if (x > 2000000000) this.n = 0; // keep x live
  if (this.n === 0) this.n = 500;
  return this.n;
};

// Hot loop => `run` tiers up through OSR with `tick` spliced in; the float
// `delta` then makes `tick` deopt every iteration (the OSR resume path).
function run(c, iters, delta) {
  var acc = 0;
  for (var i = 0; i < iters; i++) acc = acc + c.tick(delta);
  return acc;
}

// Called many times => `once` tiers up at function entry with `tick` inlined;
// the float `delta` deopts once (the entry resume path).
function once(c, delta) {
  return c.tick(delta);
}

var checksum = 0;
for (var w = 0; w < 40; w++) {
  checksum += run(new Counter(50000), 3000, 1); // warm run (OSR), int delta
}
checksum += run(new Counter(80000), 4000, 1.5); // OSR resume: float delta

for (var k = 0; k < 60000; k++) {
  checksum += once(new Counter(k & 511), 1); // warm once (entry), int delta
}
checksum += once(new Counter(7), 1.5); // entry resume: float delta

console.log("inline_resume=" + checksum);
