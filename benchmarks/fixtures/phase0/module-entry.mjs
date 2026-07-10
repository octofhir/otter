import { value } from "./module-leaf.mjs";
if (value !== 42) throw new Error("phase0 module validation failed");
