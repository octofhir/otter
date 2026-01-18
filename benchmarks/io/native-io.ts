
async function run() {
    console.log("--- Native IO Test ---");

    if (typeof Otter === "undefined" || !Otter.readFile) {
        console.log("Otter.readFile not found!");
        return;
    }

    const testFile = "test-io.txt";
    const content = "Hello Native IO! " + new Date().toISOString();

    console.log("Writing file...");
    const startWrite = performance.now();
    await Otter.writeFile(testFile, content);
    console.log(`Write took: ${(performance.now() - startWrite).toFixed(2)}ms`);

    console.log("Reading file...");
    const startRead = performance.now();
    const readBack = await Otter.readFile(testFile);
    console.log(`Read took: ${(performance.now() - startRead).toFixed(2)}ms`);

    if (readBack === content) {
        console.log("SUCCESS: Content matches!");
    } else {
        console.log("FAILURE: Content mismatch!");
        console.log("Expected:", content);
        console.log("Got:", readBack);
    }
}

run();
