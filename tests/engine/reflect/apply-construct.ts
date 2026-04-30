/* otter-test:
name = "reflect: apply / construct dispatch through the callable"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §28.1.2 Reflect.apply(target, thisArg, argumentsList).
function add(a, b) {
    return a + b;
}
if (Reflect.apply(add, null, [3, 4]) !== 7) fail();

// `this` is forwarded.
const ctx = { x: 10 };
function withThis(y) {
    return this.x + y;
}
if (Reflect.apply(withThis, ctx, [5]) !== 15) fail();

// §28.1.3 Reflect.construct(target, args).
class C {
    constructor(x) {
        this.x = x;
    }
}
const inst = Reflect.construct(C, [42]);
if (inst.x !== 42) fail();
if (!(inst instanceof C)) fail();

// Plain-function constructors work too.
function Box(v) {
    this.v = v;
}
const b = Reflect.construct(Box, [99]);
if (b.v !== 99) fail();

// Non-callable raises TypeError.
let threw = false;
try {
    Reflect.apply({}, null, []);
} catch (e) {
    threw = true;
}
if (!threw) fail();
