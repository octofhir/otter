// A hot numeric loop whose relational and additive sites observe a
// non-Number operand once per call: the first iteration reads `undefined`.
// Their feedback is not numeric, yet almost every operand is a Number.
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    var lane = index & 15;
    var threshold = index === 0 ? undefined : 8;
    var offset = index === 0 ? undefined : 3;
    if (lane > threshold) {
      checksum = checksum + 1;
    }
    var shifted = lane + offset;
    if (shifted === shifted) {
      checksum = checksum + shifted * 2;
    }
  }
  return checksum;
}
