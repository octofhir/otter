// The class's JS half: members that compose other JS surfaces are
// often clearer here than in Rust. Evaluated immediately after the
// native class installs — the two halves are one unit.
Object.defineProperty(Counter.prototype, 'describe', {
  value: function describe() {
    return `${this.label}=${this.value}`;
  },
  writable: true,
  enumerable: false,
  configurable: true,
});
