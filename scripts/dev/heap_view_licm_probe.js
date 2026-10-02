// Asm-style module: typed-array views live in the module closure and every
// access re-reads the context slot, as compiled C code does.
function Module(stdlib, buffer) {
  var HEAP8 = new stdlib.Int8Array(buffer);
  var HEAP32 = new stdlib.Int32Array(buffer);
  var top = 0;
  function sum(a, n) {
    a = a | 0;
    n = n | 0;
    var s = 0, i = 0;
    while (1) {
      s = (s + (HEAP8[(a + i) | 0] | 0)) | 0;
      s = (s + (HEAP32[((a + i) & 255) >> 2] | 0)) | 0;
      i = (i + 1) | 0;
      if ((i | 0) >= (n | 0)) break;
    }
    top = s;
    return s | 0;
  }
  return sum;
}
var sum = Module(globalThis, new ArrayBuffer(1 << 16));
var total = 0;
for (var k = 0; k < 20000; k++) total = (total + sum(k & 1023, 4096)) | 0;
console.log(total);
