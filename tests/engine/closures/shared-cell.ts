/* otter-test:
name = "closures: two closures share an upvalue cell"
[expect]
exit_code = 0
*/
function makePair() {
    let value = 10;
    function getter() {
        return value;
    }
    function setter(v) {
        value = v;
        return value;
    }
    setter(42);
    return getter();
}
makePair();
