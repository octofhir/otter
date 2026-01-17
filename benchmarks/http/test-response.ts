/**
 * Test Response.arrayBuffer() behavior in JSC
 */

console.log("Testing Response.arrayBuffer() behavior...");

// Test 1: Create response from string
const response1 = new Response("Hello World!");
console.log("Response 1 created from string:");
console.log("  - body:", response1.body);
console.log("  - bodyUsed:", response1.bodyUsed);

try {
    const buffer1 = await response1.arrayBuffer();
    console.log("  - arrayBuffer() returned:", buffer1);
    console.log("  - byteLength:", buffer1.byteLength);
    const text1 = new TextDecoder().decode(buffer1);
    console.log("  - decoded text:", text1);
} catch (e) {
    console.log("  - arrayBuffer() threw:", e);
}

// Test 2: Create response from Uint8Array
const encoder = new TextEncoder();
const bytes = encoder.encode("Test bytes");
const response2 = new Response(bytes);
console.log("\nResponse 2 created from Uint8Array:");
console.log("  - body:", response2.body);

try {
    const buffer2 = await response2.arrayBuffer();
    console.log("  - arrayBuffer() byteLength:", buffer2.byteLength);
    const text2 = new TextDecoder().decode(buffer2);
    console.log("  - decoded text:", text2);
} catch (e) {
    console.log("  - arrayBuffer() threw:", e);
}

// Test 3: Clone and read
console.log("\nTest 3: Clone and read");
const response3 = new Response("Clone test");
const cloned = response3.clone();
try {
    const text3 = await cloned.text();
    console.log("  - text():", text3);
} catch (e) {
    console.log("  - text() threw:", e);
}

console.log("\nDone");
