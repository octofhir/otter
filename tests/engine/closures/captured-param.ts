/* otter-test:
name = "closures: captured parameter"
[expect]
exit_code = 0
*/
function adder(x) {
    return function (y) {
        return x + y;
    };
}
const add5 = adder(5);
add5(3);
