import "./setup_FIXTURE.mjs";
import defer * as ns from "./dep_FIXTURE.mjs";
console.log("before", globalThis.evaluations.length);
var key = "exported";
let obj = {
  superGet(key) {
    return super[key];
  }
}
Object.setPrototypeOf(obj, ns);
obj.superGet(key);
console.log("after", globalThis.evaluations.length);
