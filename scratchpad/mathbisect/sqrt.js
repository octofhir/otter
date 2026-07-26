var s = Math.sqrt;
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    checksum = checksum + s(index & 15);
  }
  return checksum;
}
