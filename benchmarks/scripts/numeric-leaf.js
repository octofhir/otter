function engineNumericLeaf(left, right) {
  var sum = left + right;
  var product = sum * right;
  var delta = product - right;
  var offset = delta + right;
  var scaled = offset * right;
  var reduced = scaled - right;
  var quotient = reduced / right;
  return -quotient;
}

if (engineNumericLeaf(2, 2) !== -7) {
  throw new Error("numeric leaf checksum mismatch");
}

function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 100000; index = index + 1) {
    checksum = checksum + engineNumericLeaf(2, 2);
  }
  return checksum;
}
