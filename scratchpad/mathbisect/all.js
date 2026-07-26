var a = Math.abs;
var f = Math.floor;
var s = Math.sqrt;
var mx = Math.max;
var mn = Math.min;
function engineKernel() {
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    checksum = checksum + a(index & 15);
    checksum = checksum + f((index & 15) / 2);
    checksum = checksum + s(index & 15);
    checksum = checksum + mx(index & 3, 2);
    checksum = checksum + mn(index & 3, 2);
  }
  return checksum;
}
