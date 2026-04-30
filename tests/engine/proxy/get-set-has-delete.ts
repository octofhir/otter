/* otter-test:
name = "proxy: get / set / has / deleteProperty traps"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Trap dispatch shadows the target.
const target = { a: 1, b: 2 };
const log = [];
const handler = {
    get(t, k) {
        log.push("get:" + k);
        if (k === "synth") return 99;
        return t[k];
    },
    set(t, k, v) {
        log.push("set:" + k);
        t[k] = v + 100;
        return true;
    },
    has(t, k) {
        log.push("has:" + k);
        return k === "magic" || k in t;
    },
    deleteProperty(t, k) {
        log.push("del:" + k);
        delete t[k];
        return true;
    },
};
const p = new Proxy(target, handler);

if (p.a !== 1) fail();
if (p.synth !== 99) fail();
p.c = 5;
if (target.c !== 105) fail();
if (!("magic" in p)) fail();
if (!("a" in p)) fail();
if ("z" in p) fail();
delete p.a;
if (target.a !== undefined) fail();

// Missing trap → falls through to target.
const bare = new Proxy({ x: 7 }, {});
if (bare.x !== 7) fail();
