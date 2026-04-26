/* otter-test:
name = "object: nested literal access"
[expect]
exit_code = 0
*/
let o = { a: { b: { c: 42 } } };
o.a.b.c;
