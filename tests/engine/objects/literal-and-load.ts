/* otter-test:
name = "object: literal then load by name"
[expect]
exit_code = 0
*/
let o = { a: 1, b: 2, c: 3 };
o.b;
