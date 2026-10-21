function P(){ this.value = 0; }
function f(o, a, b) { o.value = (a - 1000) / b; }
var o = new P();
for (var i = 0; i < 200000; i++) f(o, 1025, 5);
console.log(o.value);
