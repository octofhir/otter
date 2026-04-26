/* otter-test:
name = "calls: recursion fib(10)"
[expect]
exit_code = 0
*/
function fib(n) {
    if (n < 2) {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}
fib(10);
