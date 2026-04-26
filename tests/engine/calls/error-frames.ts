/* otter-test:
name = "calls: runtime error reports stack frames"
[expect]
exit_code = 1
*/
function recurse() {
    return recurse();
}
recurse();
