/* otter-test:
name = "json: parse rejects trailing comma in array"
[expect]
exit_code = 1
*/
JSON.parse("[1, 2,]");
