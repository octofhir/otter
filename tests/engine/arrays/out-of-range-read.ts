/* otter-test:
name = "array: out-of-range read is undefined"
[expect]
exit_code = 0
*/
let a = [1, 2];
a[5];
