/* otter-test:
name = "proxy: apply + construct traps + getPrototypeOf"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §28.2.4.13 — `apply` trap fires on `proxy(args)`.
const fn = function (a, b) {
    return a + b;
};
const p = new Proxy(fn, {
    apply(target, thisArg, argList) {
        return target(argList[0], argList[1]) * 10;
    },
});
if (p(2, 3) !== 50) fail();

// Missing apply trap delegates to the target.
const np = new Proxy(fn, {});
if (np(2, 3) !== 5) fail();

// §28.2.4.14 — `construct` trap fires on `new proxy(args)`.
class Box {
    constructor(v) {
        this.v = v;
    }
}
const cp = new Proxy(Box, {
    construct(target, argList) {
        const inst = Reflect.construct(target, argList);
        inst.tag = "via-trap";
        return inst;
    },
});
const inst = new cp(7);
if (inst.v !== 7) fail();
if (inst.tag !== "via-trap") fail();

// §28.2.4.1 — `getPrototypeOf` trap.
const synthProto = { kind: "synth" };
const obj = {};
const gp = new Proxy(obj, {
    getPrototypeOf() {
        return synthProto;
    },
});
if (Object.getPrototypeOf(gp) !== synthProto) fail();
