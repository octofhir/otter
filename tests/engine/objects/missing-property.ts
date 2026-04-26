/* otter-test:
name = "object: missing property reads as undefined"
[expect]
exit_code = 0
*/
let o = { a: 1 };
o.zzz;
