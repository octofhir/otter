// instanceof over constructors made by `new Function`, class hierarchies,
// replaced and frozen prototypes, bound targets, @@hasInstance and
// primitives, as AST libraries use it.
var out = [];
function DEFNODE(name, base) {
  var ctor = new Function('props', 'if (props) { this.p = props; }');
  ctor.prototype = base ? Object.create(base.prototype) : {};
  ctor.prototype.TYPE = name; ctor.BASE = base;
  return ctor;
}
var Node = DEFNODE('Node'), Stmt = DEFNODE('Stmt', Node), Expr = DEFNODE('Expr', Node), Call = DEFNODE('Call', Expr);
var nodes = [new Node(1), new Stmt(2), new Expr(3), new Call(4), {}, 5, 'x', null];
var counts = [0, 0, 0, 0];
for (var i = 0; i < 40000; i++) {
  var n = nodes[i % nodes.length];
  if (n instanceof Node) counts[0]++; if (n instanceof Stmt) counts[1]++;
  if (n instanceof Expr) counts[2]++; if (n instanceof Call) counts[3]++;
}
out.push(counts.join(','));
var old = Call.prototype; Call.prototype = {}; out.push(nodes[3] instanceof Call, new Call() instanceof Call); Call.prototype = old;
class A {} class B extends A {} out.push(new B() instanceof A, new A() instanceof B);
var Bound = Expr.bind(null); out.push(nodes[3] instanceof Bound);
var Even = { [Symbol.hasInstance](v) { return v % 2 === 0; } }; out.push(4 instanceof Even, 3 instanceof Even);
var arrow = () => 1; try { ({}) instanceof arrow; } catch (e) { out.push(e.constructor.name); }
function F() {} F.prototype = 7; try { ({}) instanceof F; } catch (e) { out.push(e.constructor.name); }
out.push([] instanceof Array, (function () {}) instanceof Function, Object.create(null) instanceof Object);
console.log(out.join('\n'));
