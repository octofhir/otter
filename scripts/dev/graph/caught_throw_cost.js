"use strict";
// Exceptions thrown and caught inside one hot function, and thrown by a
// callee into a caller's catch: the caught value must flow on in compiled
// code either way.
function parse(text) {
  try {
    if (text.length % 3 === 0) throw new SyntaxError('bad ' + text.length);
    return text.length;
  } catch (e) {
    return -e.message.length;
  }
}
function risky(n) { if ((n & 7) === 0) throw n; return n; }
function guarded(n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    try { total += risky(i); } catch (v) { total -= v; }
  }
  return total;
}
const t0 = Date.now();
let sum = 0;
const words = ['a', 'ab', 'abc', 'abcd', 'abcde', 'abcdef'];
for (let i = 0; i < 2000000; i++) sum = (sum + parse(words[i % 6])) | 0;
sum = (sum + guarded(5000000)) | 0;
console.log(sum, Date.now() - t0, 'ms');
