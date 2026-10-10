function A() { this.x = 1; this.y = 2; }
A.prototype.get = function () {
  var a = this.x, b = this.y;
  a = a + b; b = a * 2; a = b - a; b = a + 3; a = b ^ a; b = a | 1; a = b & 7; b = a + this.x;
  a = a + b; b = a * 2; a = b - a; b = a + 3; a = b ^ a; b = a | 1; a = b & 7; b = a + this.y;
  a = a + b; b = a * 2; a = b - a; b = a + 3; a = b ^ a; b = a | 1; a = b & 7; b = a + this.x;
  a = a + b; b = a * 2; a = b - a; b = a + 3; a = b ^ a; b = a | 1; a = b & 7; b = a + this.y;
  return a + b;
};
A.prototype.run = function (n) {
  var s = 0;
  for (var i = 0; i < n; i++) s += this.get();
  return s;
};
var a = new A();
var t = 0;
for (var k = 0; k < 3000; k++) t += a.run(100);
console.log(t);
