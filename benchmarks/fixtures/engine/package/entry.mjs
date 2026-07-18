import { packageValue } from "#engine-dep";
if (packageValue !== 42) throw new Error("engine package validation failed");
