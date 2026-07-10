const roots = [];
for (let i = 0; i < 400; i++) roots.push({ i, text: "value-" + i, nested: [i, i + 1] });
JSON.stringify({ length: roots.length, first: roots[0].text, last: roots[399].nested[1] });
