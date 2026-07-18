var engineDoubleBox = {
  value: 0.5,
  scale: 8,
};

function engineKernel() {
  var box = engineDoubleBox;
  var checksum = 0;
  for (var index = 0; index < 1000000; index = index + 1) {
    checksum = checksum + box.value * box.scale;
  }
  return checksum;
}
