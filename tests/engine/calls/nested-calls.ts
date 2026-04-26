/* otter-test:
name = "calls: nested function calls"
[expect]
exit_code = 0
*/
function outer() {
    function inner() {
        return 42;
    }
    return inner();
}
outer();
