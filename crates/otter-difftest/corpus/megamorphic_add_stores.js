// One base constructor adds the same properties to receivers of many
// classes, so its stores go megamorphic and replay shared add-property
// transitions. A setter or a non-writable property appearing later on one
// prototype chain must stop the replay for exactly that chain.
var __extends = function (d, b) {
  function __() { this.constructor = d; }
  __.prototype = b.prototype;
  d.prototype = new __();
};
function Base(kind) {
  this.kind = kind;
  this.type = null;
  this.flags = 1;
  this.extra = kind * 2;
}
var classes = [];
for (var k = 0; k < 20; k++) {
  classes.push((function (k) {
    function Sub(x) { Base.call(this, k); this.x = x; }
    __extends(Sub, Base);
    return Sub;
  })(k));
}
var seen = 0;
function run(n, round) {
  var s = 0;
  for (var i = 0; i < n; i++) {
    var o = new classes[i % 20](i);
    s = (s + o.kind + o.extra + (o.type === null ? 1 : 7) + (o.hasOwnProperty("flags") ? 3 : 5)) | 0;
  }
  return s;
}
var out = [];
for (var r = 0; r < 6; r++) {
  if (r === 3) {
    Object.defineProperty(classes[4].prototype, "type", {
      set: function (v) { seen++; }, get: function () { return "set"; }, configurable: true,
    });
    Object.defineProperty(classes[7].prototype, "flags", { value: 9, writable: false });
  }
  out.push(run(4000, r));
}
console.log(out.join(","), seen);
