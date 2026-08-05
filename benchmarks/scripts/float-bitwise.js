function engineKernel() {
  var value = 4294967297.75;
  var acc = 0;
  var index = 0;
  var limit = 200000;
  while (index < limit) {
    var signed = value | 0;
    var shifted = value >>> (index & 7);
    acc = (acc ^ signed ^ shifted) | 0;
    value = value + 1.5;
    index = index + 1;
  }
  return acc;
}

console.log(engineKernel());
