function make(i) { return { __proto__: null, x: i, y: i + 1 }; }
var objects = new Array(100000);
for (var i = 0; i < objects.length; ++i) objects[i] = make(i);
var sum = 0;
for (var i = 0; i < objects.length; ++i) sum += objects[i].x + objects[i].y;
console.log(sum);
