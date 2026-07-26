// Property-heavy kernel with a genuinely polymorphic receiver set.
//
// The other kernels are monomorphic and fully warm, so their inline-cache
// counters read zero and they cannot measure cache work at all. This one
// cycles four distinct shapes through one load site, one prototype-chain
// load, and one store site that starts as a shape transition and then
// becomes a plain slot write.

function EnginePolyA(v) {
  this.v = v;
}
function EnginePolyB(v) {
  this.pad = 0;
  this.v = v;
}
function EnginePolyC(v) {
  this.a = 0;
  this.b = 0;
  this.v = v;
}
function EnginePolyD(v) {
  this.x = 0;
  this.y = 0;
  this.z = 0;
  this.v = v;
}

EnginePolyA.prototype.bias = 10;
EnginePolyB.prototype.bias = 20;
EnginePolyC.prototype.bias = 30;
EnginePolyD.prototype.bias = 40;

var enginePolyObjects = [
  new EnginePolyA(1),
  new EnginePolyB(2),
  new EnginePolyC(3),
  new EnginePolyD(4),
];

function engineKernel() {
  var objects = enginePolyObjects;
  var checksum = 0;
  for (var index = 0; index < 400000; index = index + 1) {
    var object = objects[index & 3];
    checksum = checksum + object.v + object.bias;
    object.acc = object.v + index;
    checksum = checksum + object.acc;
  }
  return checksum;
}
