var engineMethodReceiver = {
  bias: 4,
  apply: function (value) {
    return value + this.bias;
  },
};

function engineKernel() {
  var receiver = engineMethodReceiver;
  var checksum = 0;
  for (var index = 0; index < 1000000; index = index + 1) {
    checksum = checksum + receiver.apply(index);
  }
  return checksum;
}
