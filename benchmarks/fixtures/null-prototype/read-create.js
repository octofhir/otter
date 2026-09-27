function make(i) { var o = Object.create(null); o.x = i; o.y = i + 1; return o; }
function read(objects) { var sum = 0; for (var i = 0; i < 2000000; ++i) { var o = objects[i & 31]; sum += o.x + o.y; } return sum; }
var objects = [];
for (var i = 0; i < 32; ++i) objects.push(make(i));
console.log(read(objects));
