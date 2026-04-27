/* otter-test:
name = "array method: shift/unshift"
[expect]
exit_code = 0
*/
let a = [10, 20, 30];
a.shift();
a.unshift(99);
a.length;
a[0];
