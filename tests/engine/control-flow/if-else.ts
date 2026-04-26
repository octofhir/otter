/* otter-test:
name = "control-flow: if/else branches"
[expect]
exit_code = 0
*/
let x = 0;
if (1 < 2) {
    x = 10;
} else {
    x = 20;
}
x;
