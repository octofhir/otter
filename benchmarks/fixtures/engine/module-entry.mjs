import { value } from "./module-leaf.mjs";
if (value !== 42) throw new Error("engine module validation failed");
