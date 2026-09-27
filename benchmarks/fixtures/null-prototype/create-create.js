function make(i) { var o = Object.create(null); o.x = i; o.y = i + 1; return o; }
var objects = new Array(100000);
for (var i = 0; i < objects.length; ++i) objects[i] = make(i);
var sum = 0;
for (var i = 0; i < objects.length; ++i) sum += objects[i].x + objects[i].y;
console.log(sum);
