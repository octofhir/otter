// TypeScript-1.0-style class hierarchy: a base constructor run through
// `_super.call(this)` from many subclasses, so its `this.x = ...` stores see
// one receiver class per subclass (megamorphic property-adding stores).
var __extends = function (d, b) {
  function __() { this.constructor = d; }
  __.prototype = b.prototype;
  d.prototype = new __();
};
var Span = (function () { function Span() { this.minChar = -1; this.limChar = -1; } return Span; })();
var AST = (function (_super) {
  __extends(AST, _super);
  function AST(nodeType) {
    _super.call(this);
    this.nodeType = nodeType; this.type = null; this.flags = 1; this.passCreated = 0;
    this.preComments = null; this.postComments = null; this.docComments = null; this.isParenthesized = false;
  }
  AST.prototype.kind = function () { return this.nodeType; };
  return AST;
})(Span);
var classes = [];
for (var k = 0; k < 24; k++) {
  classes.push((function (_super, k) {
    __extends(Sub, _super);
    function Sub(x) { _super.call(this, k); this.x = x; }
    return Sub;
  })(AST, k));
}
function run(n) {
  var s = 0;
  for (var i = 0; i < n; i++) { var C = classes[i % 24]; var o = new C(i); s += o.nodeType + o.x; }
  return s;
}
var total = 0;
for (var r = 0; r < 20; r++) total += run(100000);
console.log(total);
