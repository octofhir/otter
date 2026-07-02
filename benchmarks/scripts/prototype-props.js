// Prototype data-property reads through ordinary objects. This is separate
// from richards/poly-dispatch: those stress prototype method lookup, while this
// keeps callable dispatch out of the loop and exercises direct-prototype data
// IC feedback (`obj.inheritedField`) feeding the optimizing tier.

function Shape(seed) {
  this.x = seed | 0;
  this.y = (seed * 3) | 0;
}

Shape.prototype.bias = 17;
Shape.prototype.scale = 5;
Shape.prototype.mask = 255;

var OBJECTS = new Array(128);
for (var i = 0; i < OBJECTS.length; i++) OBJECTS[i] = new Shape(i & 63);

function drive(count) {
  var acc = 0;
  for (var i = 0; i < count; i++) {
    var obj = OBJECTS[i & 127];
    acc = (acc + obj.x + obj.bias) | 0;
    acc = (acc + ((obj.y + obj.scale) & obj.mask)) | 0;
  }
  return acc;
}

var checksum = 0;
for (var run = 0; run < 40; run++) checksum = (checksum + drive(2500)) | 0;
console.log("prototype-props=" + checksum);
