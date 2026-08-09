function engineSpreadTarget(a, b) {
  return a + b;
}

function EngineSpreadBase(a, b) {
  this.total = a + b;
}

class EngineSpreadSuper {
  constructor(a, b) {
    this.total = a + b;
  }
}

class EngineSpreadDerived extends EngineSpreadSuper {
  constructor(args) {
    super(...args);
  }
}

function engineCallSpread(target, args) {
  return target(...args);
}

function engineConstructSpreadBase(Constructor, args) {
  return new Constructor(...args);
}

function engineConstructSpreadDerived(Constructor, args) {
  return new Constructor(...args);
}

var engineCallArgs = [0, 2];
var engineBaseArgs = [0, 3];
var engineSuperArgs = [0, 4];
var engineDerivedArgs = [engineSuperArgs];

for (var warmup = 0; warmup < 5000; warmup = warmup + 1) {
  engineCallArgs[0] = warmup;
  engineBaseArgs[0] = warmup;
  engineSuperArgs[0] = warmup;
  engineSpreadTarget(warmup, 2);
  new EngineSpreadBase(warmup, 3);
  new EngineSpreadDerived(engineSuperArgs);
  engineCallSpread(engineSpreadTarget, engineCallArgs);
  engineConstructSpreadBase(EngineSpreadBase, engineBaseArgs);
  engineConstructSpreadDerived(EngineSpreadDerived, engineDerivedArgs);
}

function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 100000; index = index + 1) {
    engineCallArgs[0] = index;
    engineBaseArgs[0] = index;
    engineSuperArgs[0] = index;
    checksum = checksum + engineCallSpread(engineSpreadTarget, engineCallArgs);
    checksum =
      checksum +
      engineConstructSpreadBase(EngineSpreadBase, engineBaseArgs).total;
    checksum =
      checksum +
      engineConstructSpreadDerived(EngineSpreadDerived, engineDerivedArgs).total;
  }
  return checksum;
}
