/* otter-test:
name = "generators: .return / .throw close and unwind the suspended body"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// .return(arg) closes the generator with `{value: arg, done: true}`.
function* g1() {
    yield 1;
    yield 2;
    yield 3;
}
const it = g1();
if (it.next().value !== 1) fail();
const r = it.return(99);
if (r.value !== 99 || r.done !== true) fail();
if (it.next().done !== true) fail();

// .throw(reason) routes through the body's catch handler.
function* g2() {
    try {
        yield 1;
        yield 2;
    } catch (e) {
        yield "caught:" + e;
    }
}
const it2 = g2();
it2.next();
const t = it2.throw("oops");
if (t.value !== "caught:oops") fail();
if (it2.next().done !== true) fail();

// .throw on a generator without a catch surfaces the rejection.
function* g3() { yield 1; }
const it3 = g3();
it3.next();
let threw = false;
try {
    it3.throw("bang");
} catch (e) {
    threw = true;
}
if (!threw) fail();
