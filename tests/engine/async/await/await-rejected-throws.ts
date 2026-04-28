/* otter-test:
name = "async: awaiting a rejected promise throws into the async fn"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
async function f() {
    try {
        await Promise.reject("nope");
        return "unreached";
    } catch (e) {
        return e;
    }
}
f().then((v) => {
    if (v !== "nope") fail();
});
