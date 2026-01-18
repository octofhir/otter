// Example script for building a standalone executable
// Usage: otter build examples/standalone.ts --compile -o myapp

console.log("Otter Standalone Executable Demo");
console.log("================================");
console.log();
console.log(`Platform: ${process.platform}`);
console.log(`Architecture: ${process.arch}`);
console.log(`Executable: ${process.argv[0]}`);
console.log(`Arguments: ${JSON.stringify(process.argv.slice(1))}`);
console.log();
console.log("This executable was compiled with 'otter build --compile'");
console.log("It runs without requiring Otter to be installed!");
