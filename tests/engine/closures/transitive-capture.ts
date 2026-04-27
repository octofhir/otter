/* otter-test:
name = "closures: transitive capture through three functions"
[expect]
exit_code = 0
*/
function outer() {
    let x = 100;
    function middle() {
        function inner() {
            return x;
        }
        return inner();
    }
    return middle();
}
outer();
