// The HTTP benchmark server that exits by itself after a request count, so
// end-of-run reports (JIT events, GC traces) are written.
// usage: otter http_server_limited.js <port> <requests>
const http = require('node:http');
const port = Number(process.argv[2] || 3901);
const limit = Number(process.argv[3] || 30000);
let served = 0;
const server = http.createServer((req, res) => {
  res.writeHead(200, { 'content-type': 'text/plain' });
  res.end('hello world\n');
  if (++served === limit) setImmediate(() => process.exit(0));
});
server.listen(port, () => console.log('listening', port));
