/**
 * Bun HTTP Server Benchmark
 *
 * Simple "Hello World" server for benchmarking.
 * Usage: bun run benchmarks/http/server-bun.ts
 */

const port = parseInt(process.env.PORT || "3000");

const server = Bun.serve({
    port,
    fetch(req: Request): Response {
        const url = new URL(req.url);

        if (url.pathname === "/") {
            return new Response("Hello, World!");
        }

        if (url.pathname === "/json") {
            return new Response(JSON.stringify({ message: "Hello, World!" }), {
                headers: { "Content-Type": "application/json" }
            });
        }

        if (url.pathname === "/large") {
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

console.log(`Bun server listening on http://localhost:${server.port}`);
