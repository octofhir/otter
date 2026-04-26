/* otter-test:
name = "calls: named function expression supports recursion"
[expect]
exit_code = 0
*/
let f = function fact(n) {
    if (n < 2) {
        return 1;
    }
    return n * fact(n - 1);
};
f(5);
