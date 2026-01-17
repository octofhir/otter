/**
 * Otter HTTP Server Benchmark
 *
 * Simple "Hello World" server for benchmarking.
 * Usage: otter run benchmarks/http/server-otter.ts --allow-net
 */

const port = parseInt(process.env.PORT || "3001");

const server = await Otter.serve({
    port,
    hostname: "0.0.0.0",
    fetch(req: Request): Response {
        const url = new URL(req.url);
        const pathname = url.pathname;

        if (pathname === "/") {
            return new Response("Hello, World!");
        }

        if (pathname === "/json") {
            return new Response(JSON.stringify({ message: "Hello, World!" }), {
                headers: { "Content-Type": "application/json" }
            });
        }

        if (pathname === "/large") {
            // 1KB response
            const data = {
                items: Array.from({ length: 100 }, (_, i) => ({
                    id: i,
                    name: `Item ${i}`,
                    value: Math.random()
                }))
            };
            return new Response(JSON.stringify(data), {
                headers: { "Content-Type": "application/json" }
            });
        }

        return new Response("Not Found", { status: 404 });
    }
});

console.log(`Otter server listening on http://localhost:${server.port}`);
