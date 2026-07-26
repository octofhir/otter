// Native-boundary kernel: every operation in the loop is a builtin
// implemented in Rust, reached through the JS -> native call path.
//
// The other kernels stay inside generated code and report a native-transition
// count of ~0, so they cannot measure the boundary at all. This one is nothing
// but boundary: string methods, Math, and a Map round-trip per iteration.

var engineNativeWords = [
  "otter",
  "engine",
  "polymorphic",
  "boundary",
  "runtime",
  "native",
  "property",
  "kernel",
];

function engineKernel() {
  var words = engineNativeWords;
  var table = new Map();
  var checksum = 0;
  for (var index = 0; index < 200000; index = index + 1) {
    var word = words[index & 7];
    checksum = checksum + word.length;
    checksum = checksum + word.charCodeAt(index & 3);
    checksum = checksum + word.indexOf("e");
    checksum = checksum + Math.abs(index & 15) + Math.max(index & 3, 2);
    table.set(index & 63, word);
    checksum = checksum + table.get(index & 63).length;
  }
  return checksum;
}
