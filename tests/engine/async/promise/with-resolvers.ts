/* otter-test:
name = "promise: Promise.withResolvers exposes resolve / reject pair"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// §27.2.4.6 — `{promise, resolve, reject}` triple over a fresh
// pending promise.
const a = Promise.withResolvers();
if (typeof a.resolve !== "function") fail();
if (typeof a.reject !== "function") fail();
a.promise.then((v) => {
    if (v !== "ok") fail();
});
a.resolve("ok");

const b = Promise.withResolvers();
b.promise.then(
    () => fail(),
    (e) => {
        if (e !== "bad") fail();
    }
);
b.reject("bad");
