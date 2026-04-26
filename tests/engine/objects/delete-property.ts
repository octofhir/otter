/* otter-test:
name = "object: delete removes own property"
[expect]
exit_code = 0
*/
let o = { x: 1, y: 2 };
let r1 = delete o.x;
let r2 = delete o.zzz;
r1;
r2;
o.x;
o.y;
