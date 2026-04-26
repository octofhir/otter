/* otter-test:
name = "control-flow: post-init read works (no TDZ)"
[expect]
exit_code = 0
*/
let x = 1;
x;
let y;
y = 7;
y;
