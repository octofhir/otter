// Guarded Math methods with Int32 operands and observable method identity.
// The harness owns warmup, timing, and validation of the exact checksum.
function engineKernel() {
  var math = Math;
  var checksum = 0;
  for (var index = 0; index < 400000; index = index + 1) {
    var value = (index & 15) - 8;
    checksum = checksum + math.abs(value);
    checksum = checksum + math.max(value, 2);
    checksum = checksum + math.min(value, 2);
  }
  return checksum;
}
