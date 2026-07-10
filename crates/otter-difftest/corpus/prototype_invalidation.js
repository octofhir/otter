const proto = { value: 1 };
const object = Object.create(proto);
let trace = [];
for (let i = 0; i < 100; i++) trace.push(object.value);
proto.value = 2;
object.value = 3;
delete object.value;
JSON.stringify({ before: trace[99], after: object.value, own: Object.hasOwn(object, "value") });
