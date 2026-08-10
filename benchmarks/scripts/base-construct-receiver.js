function BaseConstructReceiver(value) {
  this.value = value;
}

function constructBaseReceiver(Constructor, value) {
  return new Constructor(value);
}

for (let index = 0; index < 5000; index++) {
  constructBaseReceiver(BaseConstructReceiver, index);
}

function engineKernel() {
  let checksum = 0;
  for (let index = 0; index < 100000; index++) {
    checksum += constructBaseReceiver(BaseConstructReceiver, index).value;
  }
  return checksum;
}
