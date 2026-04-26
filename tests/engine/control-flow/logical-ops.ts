/* otter-test:
name = "control-flow: && / || / ?? short circuits"
[expect]
exit_code = 0
*/
let a = true && "hi";
let b = false || 7;
let c = null ?? "fallback";
let d = !false;
a;
b;
c;
d;
