import { packageValue } from "#phase0-dep";
if (packageValue !== 42) throw new Error("phase0 package validation failed");
