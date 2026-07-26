var mn = Math.min;
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    checksum = checksum + mn(index & 3, 2);
  }
  return checksum;
}
