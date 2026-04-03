// Example script for direct execution with the active CLI surface.
// Usage: otter run examples/standalone.ts

console.log("Otter Standalone Script Demo");
console.log("============================");
console.log();
console.log(`Platform: ${process.platform}`);
console.log(`Architecture: ${process.arch}`);
console.log(`Runtime entry: ${process.argv[0]}`);
console.log(`Arguments: ${JSON.stringify(process.argv.slice(1))}`);
console.log();
console.log("This script is meant to run through 'otter run'.");
