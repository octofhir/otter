/* otter-test:
name = "calls: extra args ignored, missing args undefined"
[expect]
exit_code = 0
*/
function f(a, b) {
    return a;
}
f(7, 8, 9);
