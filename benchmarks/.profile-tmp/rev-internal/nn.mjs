import("node:node:fs").then(m=>console.log("ok",typeof m.readFileSync),e=>console.log(e.code))
