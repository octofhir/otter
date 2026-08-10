function makePlain(offset) {
  return function plain(value) {
    let captured = value;
    function read() { return captured + offset; }
    return read();
  };
}

function makeHolder(offset) {
  return {
    method(value) {
      let captured = value;
      function read() { return captured + offset; }
      return read();
    },
  };
}

function makeBox(offset) {
  return function Box(value) {
    let captured = value;
    function read() { return captured + offset; }
    this.value = read();
  };
}

const plain = makePlain(1);
const holder = makeHolder(2);
const Box = makeBox(3);

for (let index = 0; index < 5000; index++) {
  plain(index);
  holder.method(index);
  new Box(index);
}

function engineKernel() {
  let checksum = 0;
  for (let index = 0; index < 100000; index++) {
    checksum += plain(index);
    checksum += holder.method(index);
    checksum += new Box(index).value;
  }
  return checksum;
}
