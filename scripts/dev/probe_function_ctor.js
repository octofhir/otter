var f = Function(['_', 'x'], 'return _ + x');
console.log(f(1, 2), String(f).split('\n')[0]);
var g = Function('a, b', '//# sourceURL=lodash.templateSources[0]\nreturn function(obj) {\nobj || (obj = {});\nvar __t, __p = \'\';\nwith (obj) {\n__p += \'hi \' +\n((__t = ( name )) == null ? \'\' : __t);\n}\nreturn __p\n}');
console.log(g(1, 2)({ name: 'z' }));
