// Int32 arithmetic whose first result of every call is -0 (negating lane 0)
// and whose accumulator later leaves Int32. The optimizing tier must widen
// each exiting site once and recompile, not exit on every invocation.
// The harness owns warmup, timing, and validation of the exact checksum.
function engineKernel() {
  var checksum = 0;
  var total = 2147483000;
  for (var index = 0; index < 200000; index = index + 1) {
    var lane = index & 3;
    checksum = checksum + Math.abs(-lane * 0.5) + -lane;
    total = total + lane;
  }
  return checksum + (total - 2147483000);
}
