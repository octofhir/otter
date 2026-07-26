var f = Math.floor;
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    checksum = checksum + f((index & 15) / 2);
  }
  return checksum;
}
