// Binary arguments preserve the explicit LoadProperty / CallWithThis form.
// Compare with math-intrinsics.js, which exercises fused method calls.
function engineKernel() {
  var math = Math;
  var checksum = 0;
  for (var index = 0; index < 400000; index = index + 1) {
    checksum = checksum + math.abs((index & 15) - 8);
    checksum = checksum + math.max((index & 15) - 8, 2);
    checksum = checksum + math.min((index & 15) - 8, 2);
  }
  return checksum;
}
