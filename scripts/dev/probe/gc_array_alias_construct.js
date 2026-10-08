// `new Array(n)` through a variable callee, under collection at every
// allocation: the constructed array must survive its own allocation.
var V = Array;
var out = [];
for (var round = 0; round < 40; round++) {
  var a = new V(round % 5 + 1);
  for (var i = 0; i < a.length; i++) a[i] = null;
  out.push(a.length);
}
console.log(out.join(","));
