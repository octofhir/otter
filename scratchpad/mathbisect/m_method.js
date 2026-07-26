var m = Math;
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    checksum = checksum + m.abs(index & 15);
  }
  return checksum;
}
