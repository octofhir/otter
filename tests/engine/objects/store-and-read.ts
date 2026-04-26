/* otter-test:
name = "object: store then read"
[expect]
exit_code = 0
*/
let o = {};
o.x = 7;
o.y = "hi";
o.x;
o.y;
