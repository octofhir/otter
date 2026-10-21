let token = { stamp: "identity" };
function inspect(fn) {
    const list = fn.arguments;
    const allocated = { payload: "nested-allocation" };
    return [list.length, list[0], list[1].payload, list[2] === token, allocated.payload];
}
function target(value) {
    const incremented = value + 1;
    if (value < 0 || incremented > 2147483647) return inspect(target);
    return incremented;
}
function caller(fn, value, extra, identity) {
    if (arguments.length !== 4) throw "caller arity";
    return fn(value, extra, identity);
}
for (let i = 0; i < 5000; i++) {
    target(i);
    caller(target, i, token, token);
}
console.log(JSON.stringify([
    caller(target, -1, { payload: "cold-call" }, token),
    caller(target, 2147483647, { payload: "deopt" }, token),
    caller(target, 41, token, token),
    target.arguments === null
]));
