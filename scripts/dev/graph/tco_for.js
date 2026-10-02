"use strict";
var callCount = 0;
(function f(n) {
  if (n === 0) {
    callCount += 1
    return;
  }
  for (let x = 0; ;) {
    return f(n - 1);
  }
}(100000));
console.log(callCount);
