/* otter-test:
name = "array: index store extends with undefined"
[expect]
exit_code = 0
*/
let a = [];
a[0] = 7;
a[3] = 99;
a.length;
a[1];
