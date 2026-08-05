function engineKernel() {
  var state = -1 >>> 0;
  var index = 0;
  var limit = 1000000;
  var mask = 1023;
  var factor = 3;
  while (index < limit) {
    var term = (index & mask) * factor;
    var shift = index & 7;
    state = (((state ^ term) >>> shift) | term) >>> 0;
    index = index + 1;
  }
  return state;
}
