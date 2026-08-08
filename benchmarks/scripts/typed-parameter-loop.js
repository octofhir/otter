function engineTypedParameterLoop(limit, step) {
  var total = 0;
  var index = 0;
  while (index < limit) {
    total = total + step;
    index = index + 1;
  }
  return total;
}

if (engineTypedParameterLoop(10, 3) !== 30) {
  throw new Error("typed parameter loop checksum mismatch");
}

function engineKernel() {
  return engineTypedParameterLoop(100000, 3);
}
