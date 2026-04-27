/* otter-test:
name = "closures: counter pattern"
[expect]
exit_code = 0
stdout_contains = "3"
*/
function makeCounter() {
    let n = 0;
    return function () {
        n = n + 1;
        return n;
    };
}
const counter = makeCounter();
counter();
counter();
counter();
