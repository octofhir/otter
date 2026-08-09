// Primitive string concatenation through the allocating Machine IR call ABI.
// The first result stays live across the second moving-GC safepoint.

function engineConcat3(prefix, value, suffix) {
  return (prefix + value) + suffix;
}

function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 100000; index = index + 1) {
    checksum = checksum + engineConcat3("key", 42, "!").length;
  }
  return checksum;
}
