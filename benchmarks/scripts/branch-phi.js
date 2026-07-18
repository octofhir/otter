function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 1000000; index = index + 1) {
    var contribution;
    if ((index & 1) === 0) {
      contribution = 2;
    } else {
      contribution = -14;
    }
    checksum = checksum + contribution;
  }
  return checksum;
}
