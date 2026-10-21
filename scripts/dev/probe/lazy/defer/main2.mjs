import "./setup_FIXTURE.mjs";
import defer * as ns from "./dep_FIXTURE.mjs";
let obj = { superGet(key) { return super[key]; } };
Object.setPrototypeOf(obj, ns);
console.log("v", obj.superGet("exported"), globalThis.evaluations.length);
let obj2 = { superGet(key) { return super.exported; } };
Object.setPrototypeOf(obj2, ns);
console.log("v2", obj2.superGet("exported"), globalThis.evaluations.length);
