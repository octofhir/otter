// A hot numeric loop with one relational site whose right operand is a
// String. That site's feedback is not numeric, so the optimizing tier must
// complete it through the generic operator rather than decline the function.
var engineThreshold = "8";

function engineKernel() {
  var threshold = engineThreshold;
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    var lane = index & 15;
    var weight = (lane * 3 + 1) & 7;
    checksum = checksum + weight * lane - (lane >> 1);
    if (lane > threshold) {
      checksum = checksum + 1;
    }
  }
  return checksum;
}
