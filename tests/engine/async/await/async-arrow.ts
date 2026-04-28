/* otter-test:
name = "async: async arrow function follows the same lowering"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const f = async (x) => {
    let y = await Promise.resolve(x);
    return y * 2;
};
f(5).then((v) => {
    if (v !== 10) fail();
});
