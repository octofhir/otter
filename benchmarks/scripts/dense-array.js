var engineDenseValues = [1, 2, 3, 4, 5, 6, 7, 8];

function engineKernel() {
  var values = engineDenseValues;
  var checksum = 0;
  for (var index = 0; index < 1163264; index = index + 1) {
    checksum = checksum + values[index & 7];
  }
  return checksum;
}
