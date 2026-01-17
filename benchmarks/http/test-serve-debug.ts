/**
 * Debug test for Otter.serve()
 */

console.log("Starting Otter.serve() debug test...");

const server = await Otter.serve({
    port: 3001,
    hostname: "127.0.0.1",
    fetch(req: Request): Response {
        console.log(`Received: ${req.method} ${req.url}`);
        const body = "Hello from Otter!";
        console.log(`Sending response with body: "${body}" (length: ${body.length})`);
        const response = new Response(body);
        console.log(`Response created, body: ${response.body}, status: ${response.status}`);
        return response;
    }
});

console.log(`Server listening on http://${server.hostname}:${server.port}`);
console.log("Press Ctrl+C to stop");
