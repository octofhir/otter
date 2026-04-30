/* otter-test:
name = "proxy: missing traps fall through to target object"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const target = { a: 1, b: 2, c: 3 };
const p = new Proxy(target, {});

// Without traps, every operation hits the target.
if (p.a !== 1) fail();
p.d = 4;
if (target.d !== 4) fail();
if (!("a" in p)) fail();
delete p.a;
if (target.a !== undefined) fail();
