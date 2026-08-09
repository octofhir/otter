class EngineBase {
  constructor(value) {
    this.base = value + 1;
  }
}

class EngineDerived extends EngineBase {
  constructor(value) {
    super(value);
    this.derived = value + 2;
  }
}

for (var warmup = 0; warmup < 5000; warmup = warmup + 1) {
  new EngineDerived(warmup);
}

function engineConstructDerived(Constructor, value) {
  return new Constructor(value);
}

function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 100000; index = index + 1) {
    var instance = engineConstructDerived(EngineDerived, index);
    checksum = checksum + instance.base + instance.derived;
  }
  return checksum;
}
