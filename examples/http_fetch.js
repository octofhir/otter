(async () => {
  const response = await fetch("https://example.com", { method: "GET" });
  console.log("fetch status", response.status, response.ok);
  const text = await response.text();
  console.log("body length", text.length);
  const clone = response.clone();
  const buffer = await clone.arrayBuffer();
  console.log("arrayBuffer size", buffer.byteLength);
})();
