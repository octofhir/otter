function engineKernel() {
  var value = 305419896;
  var mask = 2147483647;
  for (var index = 0; index < 1000000; index = index + 1) {
    var left = value << index;
    var right = value >> 5;
    value = (left ^ right) | index;
    value = value & mask;
    value = ~value;
  }
  return value;
}
