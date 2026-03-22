// JIT benchmark: mixed object+array+string server-style workload
// Measures: realistic mixed-type hot paths, allocation pressure, IC diversity

interface Request {
  method: string;
  path: string;
  headers: Record<string, string>;
  body: string | null;
}

interface Response {
  status: number;
  body: string;
  headers: Record<string, string>;
}

function parseQueryString(path: string): Record<string, string> {
  const result: Record<string, string> = {};
  const qIndex = path.indexOf("?");
  if (qIndex === -1) return result;
  const query = path.slice(qIndex + 1);
  const pairs = query.split("&");
  for (let i = 0; i < pairs.length; i++) {
    const eq = pairs[i].indexOf("=");
    if (eq !== -1) {
      result[pairs[i].slice(0, eq)] = pairs[i].slice(eq + 1);
    }
  }
  return result;
}

function handleRequest(req: Request): Response {
  const params = parseQueryString(req.path);
  const headers: Record<string, string> = {
    "content-type": "application/json",
  };

  if (req.method === "GET") {
    const id = params["id"] || "0";
    return {
      status: 200,
      body: `{"id":${id},"name":"item"}`,
      headers,
    };
  } else if (req.method === "POST") {
    return {
      status: 201,
      body: '{"created":true}',
      headers,
    };
  }
  return { status: 404, body: '{"error":"not found"}', headers };
}

function processRequests(requests: Request[]): number {
  let totalStatus = 0;
  for (let i = 0; i < requests.length; i++) {
    const resp = handleRequest(requests[i]);
    totalStatus += resp.status;
  }
  return totalStatus;
}

// Generate a batch of requests
function generateRequests(n: number): Request[] {
  const reqs: Request[] = [];
  for (let i = 0; i < n; i++) {
    if (i % 3 === 0) {
      reqs.push({
        method: "GET",
        path: `/api/items?id=${i}&page=1`,
        headers: { accept: "application/json" },
        body: null,
      });
    } else if (i % 3 === 1) {
      reqs.push({
        method: "POST",
        path: "/api/items",
        headers: {
          accept: "application/json",
          "content-type": "application/json",
        },
        body: `{"name":"item${i}"}`,
      });
    } else {
      reqs.push({
        method: "DELETE",
        path: `/api/items/${i}`,
        headers: { accept: "*/*" },
        body: null,
      });
    }
  }
  return reqs;
}

const requests = generateRequests(1000);
const ITERS = 500;

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  processRequests(requests);
}
const elapsed = Date.now() - start;
console.log(`mixed_server: ${elapsed}ms (${ITERS} iterations, ${requests.length} requests/iter)`);
