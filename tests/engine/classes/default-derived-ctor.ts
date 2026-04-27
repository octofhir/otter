/* otter-test:
name = "classes: derived class without ctor calls super() automatically"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
class Base {
    constructor() {
        this.tag = "base";
    }
    where() {
        return this.tag;
    }
}
// Derived class with no explicit constructor — the foundation
// synthesises `constructor() { super(); }`.
class Sub extends Base {
    extra() {
        return this.where() + "/sub";
    }
}
const s = new Sub();
if (s.tag !== "base") fail();
if (s.extra() !== "base/sub") fail();
