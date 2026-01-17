/**
 * Simple test for Otter.serve()
 */

console.log("Starting Otter.serve() test...");

const server = await Otter.serve({
    port: 3001,
    hostname: "127.0.0.1",
    fetch(req: Request): Response {
        console.log(`${req.method} ${req.url}`);
        return new Response("Hello from Otter!");
    }
});

console.log(`Server listening on http://${server.hostname}:${server.port}`);
console.log("Press Ctrl+C to stop");
