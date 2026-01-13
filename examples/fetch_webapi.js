(async () => {
  const headers = new Headers({ "x-demo": "otter" });
  headers.append("x-demo", "extra");

  const params = new URLSearchParams({ q: "otter", page: "1" });
  const response = await fetch(`https://example.com/?${params.toString()}`, {
    method: "GET",
    headers,
  });

  console.log("status", response.status, response.ok);
  console.log("header x-demo", headers.get("x-demo"));
  const text = await response.text();
  console.log("text length", text.length);

  const clone = response.clone();
  const buffer = await clone.arrayBuffer();
  console.log("clone arrayBuffer length", buffer.byteLength);

  const form = new FormData();
  form.append("name", "otter");
  form.append("kind", "runtime");
  console.log("form data", form.toString());
})();
