/* otter-test:
name = "json: parse rejects leading zero on integer literal"
[expect]
exit_code = 1
*/
JSON.parse("01");
