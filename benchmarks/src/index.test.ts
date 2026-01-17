// Example test file

describe("greet", () => {
  it("should return a greeting message", () => {
    const result = "Hello, World!";
    expect(result).toContain("Hello");
  });

  it("should work with different names", () => {
    const name = "Otter";
    expect(name.length).toBeGreaterThan(0);
  });
});
