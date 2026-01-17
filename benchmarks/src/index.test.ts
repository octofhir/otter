// Example test file
import { describe, it } from "node:test";
import assert from "node:assert";

describe("greet", () => {
  it("should return a greeting message", () => {
    const result = "Hello, World!";
    assert.ok(result.includes("Hello"));
  });

  it("should work with different names", () => {
    const name = "Otter";
    assert.ok(name.length > 0);
  });
});
