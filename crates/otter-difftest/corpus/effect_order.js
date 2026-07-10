const trace = [];
const key = { toString() { trace.push("key"); return "x"; } };
const object = { get x() { trace.push("get"); return 7; } };
const value = object[key];
Promise.resolve().then(() => trace.push("microtask"));
JSON.stringify({ value, trace });
