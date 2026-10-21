import "./setup_FIXTURE.mjs";
import defer * as ns from "./dep_FIXTURE.mjs";
console.log("eager", eval('let o = { superGet(key){ return super[key]; } }; Object.setPrototypeOf(o, ns); o.superGet("exported")'), globalThis.evaluations.length);
