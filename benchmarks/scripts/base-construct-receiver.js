function BaseConstructReceiver(left) {
  this.left = left;
  this.right = 7;
}

class DerivedConstructReceiver extends BaseConstructReceiver {
  constructor(left) {
    super(left);
  }
}

function constructBaseReceiver(Constructor, left) {
  return new Constructor(left);
}

function constructSpreadReceiver(Constructor, args) {
  return new Constructor(...args);
}

function constructDerivedReceiver(Constructor, left) {
  return new Constructor(left);
}

for (let index = 0; index < 5000; index++) {
  new DerivedConstructReceiver(index);
}

for (let index = 0; index < 5000; index++) {
  constructBaseReceiver(BaseConstructReceiver, index);
  constructSpreadReceiver(BaseConstructReceiver, [index]);
  constructDerivedReceiver(DerivedConstructReceiver, index);
}

function engineKernel() {
  let checksum = 0;
  const spreadArgs = [0];
  for (let index = 0; index < 100000; index++) {
    spreadArgs[0] = index;
    const fixed = constructBaseReceiver(BaseConstructReceiver, index);
    const spread = constructSpreadReceiver(BaseConstructReceiver, spreadArgs);
    const derived = constructDerivedReceiver(DerivedConstructReceiver, index);
    checksum += fixed.left + fixed.right;
    checksum += spread.left + spread.right;
    checksum += derived.left + derived.right;
  }
  return checksum;
}
