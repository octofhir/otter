function EngineBoundaryBase(value) {
  this.total = value + 1;
}

function engineScalarBoundary(value) {
  var array = [value];
  var tag = typeof value;
  var length = array.length;
  var text = "otter";
  var character = text[1];
  var bigintSame = 1n === 1n;
  return (
    value +
    length +
    (tag === "number" ? 1 : 0) +
    (character === "t" ? 1 : 0) +
    (bigintSame ? 1 : 0)
  );
}

function engineClassBoundary(Base, key, value) {
  class Local extends Base {
    [key]() {
      return value + 1;
    }
  }
  return typeof Local === "function" ? value + 1 : 0;
}

for (var warmup = 0; warmup < 5000; warmup = warmup + 1) {
  engineScalarBoundary(warmup);
  engineClassBoundary(EngineBoundaryBase, "bump", warmup);
}

function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 100000; index = index + 1) {
    checksum = checksum + engineScalarBoundary(index);
    checksum =
      checksum + engineClassBoundary(EngineBoundaryBase, "bump", index);
  }
  return checksum;
}
